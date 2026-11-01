#!/usr/bin/env python3
"""Run production pending-owner FIFO against VecDeque, including wrapped drops."""
from pathlib import Path
import subprocess,tempfile
from g17_compute_inflight_test_support import pair_fifo_source
code=r'''
#![allow(dead_code)]
use std::{collections::VecDeque,sync::{Arc,atomic::{AtomicUsize,Ordering}}};
''' + pair_fifo_source() + r'''
fn exercise<const N:usize>(){
 let mut q=PairFifo::<u64,N>::new();let mut oracle=VecDeque::new();let mut seed=0x12345678u64;
 for value in 0..100000{
  seed=seed.wrapping_mul(6364136223846793005).wrapping_add(1);
  match (seed>>32)%7{
   0|1|2=>{let accepted=oracle.len()<N;assert_eq!(q.push(value).is_ok(),accepted);if accepted{oracle.push_back(value);}},
   3=>assert_eq!(q.take(),oracle.pop_front()),
   4=>assert_eq!(q.pop_back(),oracle.pop_back()),
   5=>{if let Some(x)=q.back_mut(){*x+=1;}if let Some(x)=oracle.back_mut(){*x+=1;}},
   _=>{for x in q.iter_mut(){*x=x.wrapping_add(2);}for x in oracle.iter_mut(){*x=x.wrapping_add(2);}},
  }
  assert_eq!(q.len(),oracle.len());assert_eq!(q.has_room(),oracle.len()<N);
  assert_eq!(q.front(),oracle.front());assert_eq!(q.back(),oracle.back());
  assert_eq!(q.iter().copied().collect::<Vec<_>>(),oracle.iter().copied().collect::<Vec<_>>());
  if value%307==0{q.clear();oracle.clear();assert!(q.is_none());}
 }
}
#[test]fn all_capacities_preserve_order_across_wrap(){exercise::<0>();exercise::<1>();exercise::<2>();exercise::<7>();exercise::<31>();exercise::<32>();}
struct DropOnce(Arc<AtomicUsize>);impl Drop for DropOnce{fn drop(&mut self){self.0.fetch_add(1,Ordering::SeqCst);}}
#[test]fn wrapped_clear_rollback_and_drop_release_each_owner_once(){let count=Arc::new(AtomicUsize::new(0));{let mut q=PairFifo::<DropOnce,3>::new();for _ in 0..3{assert!(q.push(DropOnce(count.clone())).is_ok());}drop(q.take());assert!(q.push(DropOnce(count.clone())).is_ok());drop(q.pop_back());q.clear();assert_eq!(count.load(Ordering::SeqCst),4);assert!(q.push(DropOnce(count.clone())).is_ok());}assert_eq!(count.load(Ordering::SeqCst),5);}
'''
with tempfile.TemporaryDirectory() as tmp:
 p=Path(tmp)/'fifo.rs';p.write_text(code);exe=Path(tmp)/'fifo'
 subprocess.run(['rustc','--edition=2021','--test',str(p),'-o',str(exe)],check=True)
 subprocess.run([str(exe)],check=True)
print('PASS production FIFO: 600000 reference-model operations, wrap, rollback, iteration, zero capacity and exact drops')
