#!/usr/bin/env python3
"""Actual unbind helper must reject an IRQ enqueue from a stale armed read."""
from pathlib import Path
import subprocess,sys,tempfile
s=(Path(__file__).resolve().parents[1]/'driver.rs').read_text()
a=s.index('fn disable_g17p_work<');b=s.index('{',a)+1;depth=1
while depth:depth+=(s[b]=='{')-(s[b]=='}');b+=1
helper=s[a:b]
if '--negative-control' in sys.argv:helper=helper.replace('bindings::disable_work_sync(work)','bindings::cancel_work_sync(work)')
rust=r'''
#![allow(dead_code)]
use std::sync::atomic::{AtomicBool,Ordering};
struct RawWork{disabled:AtomicBool,pending:AtomicBool}
impl RawWork{fn enqueue(&self)->bool{if self.disabled.load(Ordering::SeqCst){false}else{self.pending.store(true,Ordering::SeqCst);true}}}
struct Work<T,const ID:u64>{raw:RawWork,marker:std::marker::PhantomData<fn()->T>}
impl<T,const ID:u64> Work<T,ID>{unsafe fn raw_get(work:*mut Self)->*mut RawWork{unsafe{std::ptr::addr_of_mut!((*work).raw)}}}
trait HasWork<T,const ID:u64>{unsafe fn raw_get_work(dev:*mut T)->*mut Work<T,ID>;}
struct AsahiDevice{work:Work<Self,1>}
impl HasWork<AsahiDevice,1> for AsahiDevice{unsafe fn raw_get_work(dev:*mut AsahiDevice)->*mut Work<AsahiDevice,1>{unsafe{std::ptr::addr_of_mut!((*dev).work)}}}
mod bindings{
 pub unsafe fn disable_work_sync(work:*mut super::RawWork)->bool{let work=unsafe{&*work};work.disabled.store(true,super::Ordering::SeqCst);work.pending.swap(false,super::Ordering::SeqCst)}
 pub unsafe fn cancel_work_sync(work:*mut super::RawWork)->bool{unsafe{&*work}.pending.swap(false,super::Ordering::SeqCst)}
}
'''+helper+r'''
#[test]fn stale_irq_cannot_requeue_after_unbind_join(){
 let dev=AsahiDevice{work:Work{raw:RawWork{disabled:AtomicBool::new(false),pending:AtomicBool::new(false)},marker:std::marker::PhantomData}};
 assert!(dev.work.raw.enqueue());
 // The IRQ already observed armed=true before ingress was closed. It can
 // only call queue_work now: permanent disabling must reject that enqueue.
 disable_g17p_work::<1>(&dev);
 assert!(!dev.work.raw.pending.load(Ordering::SeqCst));
 assert!(!dev.work.raw.enqueue(),"stale IRQ can execute after runtime teardown");
}
'''
with tempfile.TemporaryDirectory() as d:
 p=Path(d)/'test.rs';p.write_text(rust);binary=Path(d)/'test'
 subprocess.run(['rustc','--edition=2021','--test',str(p),'-o',str(binary)],check=True)
 subprocess.run([str(binary)],check=True)
for i in [1,2,3,4]:assert f'disable_g17p_work::<{i}>' in s
