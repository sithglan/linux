#!/usr/bin/env python3
"""Compile exact buffer/formatter callbacks with kernel FFI and privileged clock reads mocked."""
from pathlib import Path
import subprocess, tempfile
root = Path(__file__).resolve().parents[1]
s = (root/'g17_timing_buffer.rs').read_text()
s = s[s.index('use core::sync'):s.index('static OPS:')]
s = s.replace('use kernel::{bindings, module_param::KernelParam};', '')
s = s.replace('const CAPACITY: usize = 16384;', 'const CAPACITY: usize = 128;')
# CNTPCT is privileged on the Linux test host; preserve the actual callbacks,
# replacing only the hardware reads (not the diagnostic command guard).
for name, value in [('arch_counter', 1000000000), ('arch_frequency', 1000000000)]:
    start=s.index('pub(crate) fn '+name+'(')
    at=s.index('{',start)+1; depth=1
    while depth:
        depth += (s[at]=='{') - (s[at]=='}'); at+=1
    s=s[:start]+f'pub(crate) fn {name}() -> u64 {{ {value} }}'+s[at:]

mock = r'''
#[macro_export] macro_rules! pr_info { ($($arg:tt)*) => { let _ = format_args!($($arg)*); }; }
mod kernel { pub use crate::pr_info; pub mod time {pub struct Monotonic; pub trait ClockSource{fn ktime_get()->i64;} impl ClockSource for Monotonic{fn ktime_get()->i64{1}}}}
mod g17_manager {
 pub static CALLS:std::sync::atomic::AtomicUsize=std::sync::atomic::AtomicUsize::new(0);
 #[derive(Debug,Copy,Clone)] pub struct Error; impl Error{pub fn to_errno(self)->i32{-11}}
 pub fn diagnostic_accounting_clocks()->Result<[u64;8],Error>{CALLS.fetch_add(1,std::sync::atomic::Ordering::Relaxed);Ok([0;8])}
}
mod bindings {
 #[allow(non_camel_case_types)] pub struct kernel_param;
 pub unsafe fn ktime_get()->i64{1}
}
mod module_parameters {
 pub struct Param; impl Param{pub fn value(&self)->&u32{&0}}
 pub static g17p_completion_trace:Param=Param; pub static g17p_trace_depth:Param=Param;
}
fn command(s:&std::ffi::CStr)->i32{unsafe{set(s.as_ptr(),std::ptr::null())}}
fn page()->String{let mut bytes=[0u8;4096]; let len=unsafe{get(bytes.as_mut_ptr().cast(),std::ptr::null())} as usize; assert!(len<4096); String::from_utf8(bytes[..len].to_vec()).unwrap()}
fn reset(){STATE.store(0,Ordering::Relaxed); NEXT.store(FROZEN,Ordering::Relaxed);CURSOR.store(0,Ordering::Relaxed);FROZEN_BOUND.store(0,Ordering::Relaxed);DROPPED.store(0,Ordering::Relaxed);for slot in &SLOTS{slot.ready.store(0,Ordering::Relaxed);}}
fn main(){
 assert_eq!(command(c"clock"),-22); assert_eq!(command(c"0"),0); assert_eq!(command(c"clock"),-22); reset();
 record([99;WORDS]); assert_eq!(reserved(),0); assert_eq!(command(c"bad"),-22);
 assert_eq!(command(c"1"),0); assert_eq!(command(c"1"),-16);
 // Max-width formatter remains bounded; fixed-size buffer fills without overwrite.
 for _ in 0..CAPACITY+7{record([u64::MAX;WORDS]);}
 assert_eq!(DROPPED.load(Ordering::Relaxed),7);
 assert_eq!(command(c"clock"),-22);
 assert_eq!(command(c"0"),0); assert_eq!(command(c"clock"),0); assert_eq!(g17_manager::CALLS.load(Ordering::Relaxed),1); let p=page(); assert_eq!(p.lines().filter(|s|s.starts_with("record")).count(),4);
 assert_eq!(command(c"rewind"),0);assert_eq!(page(),p);
 assert_eq!(command(c"1"),-16); assert_eq!(reserved(),CAPACITY);
 reset();assert_eq!(command(c"1"),0);
 // Model a writer preempted immediately after the actual reservation.
 let index=NEXT.fetch_add(1,Ordering::Relaxed);assert_eq!(command(c"0"),0);
 assert_eq!(reserved(),1);assert!(page().contains("cursor=0 reserved=1"));
 for word in &SLOTS[index].words{word.store(123,Ordering::Relaxed);}
 SLOTS[index].ready.store(1,Ordering::Release);assert!(page().contains("cursor=1 reserved=1"));
 // Repeated concurrent producers and stop/drain: no torn records, no tickets lost.
 for _ in 0..100 {
  reset();assert_eq!(command(c"1"),0);
  let threads:Vec<_>=(1..9).map(|id|std::thread::spawn(move||{for _ in 0..40{record([id;WORDS]);std::thread::yield_now();}})).collect();
  std::thread::yield_now();assert_eq!(command(c"0"),0);
  for t in threads{t.join().unwrap();}
  let mut records=0;
  while CURSOR.load(Ordering::Relaxed)<reserved(){
   for line in page().lines().filter(|l|l.starts_with("record")){
    let fields:Vec<_>=line.split_whitespace().collect();assert_eq!(fields.len(),WORDS+2);
    assert!(fields[2..].iter().all(|f|*f==fields[2]));records+=1;
   }
  }
  assert_eq!(records,reserved());
 }
 println!("PASS: disabled, controls, capacity/drop, PAGE_SIZE, rewind retention, pending reservation, 100 concurrent freeze/drain rounds");
}
'''
with tempfile.TemporaryDirectory() as d:
 p=Path(d); (p/'test.rs').write_text(s+mock)
 subprocess.run(['rustc','--edition=2021','-O',str(p/'test.rs'),'-o',str(p/'test')],check=True)
 subprocess.run([str(p/'test')],check=True)
