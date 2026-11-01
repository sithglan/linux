#!/usr/bin/env python3
"""v90: execute the production pooled firmware-cached object lifecycle.

Extracted verbatim from the driver: the lock-free pool slots, the family
index, `MappedObject::new_fw_pooled`, `impl Drop for MappedObject`,
`g17p_fw_pool_take`, `g17p_fw_pool_release_all`, the statistics helpers,
`mmu::KernelMapping::set_fw_cached_prefix` (against a mock page table), and
the runtime family gate. The mocks count every KernelMapping drop (= an
unmap). The invariant under test: a mapping that ever carried the cached
attribute is never unmapped while pooled use continues, is reused only zeroed
and with identical geometry, and is released only after the originating firmware is proven stopped.
"""
from pathlib import Path
import json, subprocess, tempfile

d = Path(__file__).resolve().parents[1]
res = (d / 'g17_resources.rs').read_text()
mmu = (d / 'mmu.rs').read_text()
tb = (d / 'g17_timing_buffer.rs').read_text()


def item(text, marker):
    start = text.index(marker)
    end = text.index('{', start) + 1
    depth = 1
    while depth:
        depth += (text[end] == '{') - (text[end] == '}')
        end += 1
    return text[start:end]


def line(text, marker):
    start = text.index(marker)
    return text[start:text.index(';\n', start) + 1]


pool_src = '\n'.join([
    '\n'.join(line(res, 'pub(crate) const ' + n) for n in [
        'G17P_FW_FAMILY_DESCRIPTOR', 'G17P_FW_FAMILY_TSQ_NODE', 'G17P_FW_FAMILY_CHANNEL_STATE',
        'G17P_FW_FAMILY_CONFIG_UPDATE']),
    line(res, 'const G17P_FW_POOL_POISON'), line(res, 'const G17P_FW_POOL_SLOTS'),
    line(res, 'const G17P_FW_POOL_CACHED_LIMIT'),
    item(res, 'pub(crate) const fn g17p_fw_pool_index('),
    item(res, 'pub(crate) struct G17PFwPoolSlots<'),
    item(res, 'impl<T, const N: usize> G17PFwPoolSlots<T, N> {'),
    item(res, 'struct G17PPooledFwObject {'),
    line(res, 'static G17P_FW_POOLS:'), line(res, 'static G17P_FW_POOL_CACHED_BYTES'),
    line(res, 'static G17P_FW_POOL_STATS'), line(res, 'static G17P_FW_POOL_RESERVED'),
    item(res, 'fn g17p_fw_pool_stat('), item(res, 'fn g17p_fw_pool_bump('),
    item(res, 'impl Drop for MappedObject {'),
    item(res, 'fn g17p_fw_pool_take('),
    item(res, 'pub(crate) fn g17p_fw_pool_release_all('),
])
method = item(res, '    fn new_fw_pooled(')
prefix_method = item(mmu, '    pub(crate) unsafe fn set_fw_cached_prefix(')
gate = item(tb, 'pub(crate) fn fw_cached_family_mask(')
owner_struct = line(mmu, 'pub(crate) struct FirmwareCacheOwner(')
owner_impl = item(mmu, 'impl FirmwareCacheOwner {')
# Scheduling hook after the real production stop-load: return the sampled value,
# while allowing another final-stop thread to start its sweep at that boundary.
owner_impl = owner_impl.replace('self.0.firmware_cache_stopped.load(Ordering::SeqCst)',
    '{ let sampled = self.0.firmware_cache_stopped.load(Ordering::SeqCst); '
    'test_stop_read_hook(); sampled }')
owner_method = item(mmu, '    pub(crate) fn firmware_cache_owner(')
stop_method = item(mmu, '    pub(crate) fn mark_firmware_cache_stopped(')

source = r'''
#![allow(dead_code, unused_variables, unused_macros, unused_imports)]
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, AtomicBool, Ordering, fence};
use std::cell::RefCell; use std::ops::Range;
type Result<T=()> = std::result::Result<T,i32>;
const EINVAL:i32=22; const EFAULT:i32=14; const EBUSY:i32=16;
const UAT_PGSZ:usize=16384;
macro_rules! dev_info { ($($x:tt)*) => { () } }
macro_rules! dev_err { ($($x:tt)*) => { () } }
static UNMAPS: AtomicUsize = AtomicUsize::new(0);
static PROOF_MODE: AtomicU32 = AtomicU32::new(0); // 0 ok, 1 refuse(no mutation), 2 fail(mutated)
static PROOF_CALLS: AtomicUsize = AtomicUsize::new(0);
static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static NEXT_IOVA: AtomicU64 = AtomicU64::new(0x1000_0000);
static DIAG_KNOBS: AtomicU32 = AtomicU32::new(0);
mod g17_timing_buffer { use super::*;
''' + gate + r'''
}
mod crate_shim {}
static STOP_READ_HOOK: std::sync::Mutex<Option<(std::sync::Arc<std::sync::Barrier>,std::sync::Arc<std::sync::Barrier>)>> = std::sync::Mutex::new(None);
fn test_stop_read_hook() { let hook=STOP_READ_HOOK.lock().unwrap().take();if let Some((entered,resume))=hook {entered.wait();resume.wait();} }
static FAIL_POOL_NODE:AtomicBool=AtomicBool::new(false);
struct PoolLock(std::sync::Mutex<()>);
impl PoolLock { fn lock(&self)->std::sync::MutexGuard<'_,()> {self.0.lock().unwrap()} }
static G17P_FW_POOL_LOCK:PoolLock=PoolLock(std::sync::Mutex::new(()));
struct GfpFlags; const GFP_KERNEL: GfpFlags = GfpFlags;
struct KBox<T>(Box<T>);
impl<T> std::ops::Deref for KBox<T> {type Target=T;fn deref(&self)->&T {&self.0}}
impl<T> std::ops::DerefMut for KBox<T> {fn deref_mut(&mut self)->&mut T {&mut self.0}}
impl<T> KBox<T> {
 fn new(v:T,_:GfpFlags)->std::result::Result<Self,()> {if FAIL_POOL_NODE.swap(false,Ordering::SeqCst) {Err(())} else {Ok(KBox(Box::new(v)))}}
 fn new_uninit(_: GfpFlags) -> std::result::Result<KBox<std::mem::MaybeUninit<T>>, ()> { Ok(KBox(Box::new(std::mem::MaybeUninit::uninit()))) }
 fn into_raw(b: Self) -> *mut T { Box::into_raw(b.0) }
 unsafe fn from_raw(p: *mut T) -> Self { KBox(unsafe { Box::from_raw(p) }) }
 fn into_inner(b: Self) -> T { *b.0 }
}
impl<T> KBox<std::mem::MaybeUninit<T>> { fn write(b: Self, v: T) -> KBox<T> { let mut b = b.0; b.write(v); KBox(unsafe { Box::from_raw(Box::into_raw(b).cast::<T>()) }) } }
mod kernel { pub mod sync { pub mod aref { pub type ARef<T> = std::sync::Arc<T>; } } }
mod gem { pub struct Object; pub struct ObjectRef { pub gem: std::sync::Arc<Object> }
  impl ObjectRef { pub fn new(gem: std::sync::Arc<Object>) -> Self { ObjectRef { gem } } } }
mod mmu {
 use super::*;
 use std::sync::Arc;
 pub struct UatInner { pub firmware_cache_stopped: AtomicBool }
 #[derive(Clone)] pub struct Uat { pub inner: Arc<UatInner> }
 impl Uat {
  pub fn new()->Self {Self{inner:Arc::new(UatInner{firmware_cache_stopped:AtomicBool::new(false)})}}
''' + stop_method + r'''
 }
 pub struct MappingInner { pub iova:u64, pub size:usize, pub uat_inner:Arc<UatInner> }
 pub struct KernelMapping(pub MappingInner);
 impl Drop for KernelMapping { fn drop(&mut self) { UNMAPS.fetch_add(1, Ordering::SeqCst); } }
''' + owner_struct + owner_impl + r'''
 impl KernelMapping {
''' + owner_method + r'''
  pub unsafe fn set_fw_cached_prefix(&self, prefix: usize) -> (Result<[u64; 2]>, bool) {
   PROOF_CALLS.fetch_add(1, Ordering::SeqCst);
   assert!(prefix > 0 && prefix <= self.0.size);
   match PROOF_MODE.load(Ordering::SeqCst) { 0 => (Ok([0x40b, 0x44b]), false), 1 => (Err(EINVAL), false), _ => (Err(EFAULT), true) }
  }
 }
}
thread_local! {static DEFAULT_UAT:RefCell<mmu::Uat>=RefCell::new(mmu::Uat::new());}
fn default_uat()->mmu::Uat {DEFAULT_UAT.with(|u|u.borrow().clone())}
fn stop_default(){default_uat().mark_firmware_cache_stopped();}

struct AsahiDevice; impl AsahiDevice { fn as_ref(&self) -> &Self { self } }
struct MappedObject { mapping: Option<mmu::KernelMapping>, object: gem::ObjectRef, iova: u64,
  object_offset: usize, logical_size: usize, cpu_wc: bool, fw_pool: u8, fw_node:Option<KBox<G17PPooledFwObject>>, bytes: RefCell<Vec<u8>> }
impl MappedObject {
 fn plain(size: usize, cpu_wc: bool) -> Result<Self> {
  ALLOCS.fetch_add(1, Ordering::SeqCst);
  let iova = NEXT_IOVA.fetch_add(0x100000, Ordering::SeqCst);
  Ok(MappedObject { mapping: Some(mmu::KernelMapping(mmu::MappingInner { iova, size, uat_inner:default_uat().inner })), object: gem::ObjectRef::new(std::sync::Arc::new(gem::Object)),
     iova, object_offset: 0, logical_size: size, cpu_wc, fw_pool: 0, fw_node:None, bytes: RefCell::new(vec![0; size]) })
 }
 fn with_bytes_mut<R>(&mut self, f: impl FnOnce(&mut [u8]) -> Result<R>) -> Result<R> { f(&mut self.bytes.borrow_mut()) }
''' + method + r'''
}
'''
# The production take() constructs MappedObject without the test-only byte
# model; give the mock a default for it.
pool_src = pool_src.replace('            fw_node: Some(entry),', '            bytes: RefCell::new(vec![0xa5; entry.logical_size]),\n            fw_node: Some(entry),')
source += pool_src + r'''
fn reset() {
 DEFAULT_UAT.with(|u|*u.borrow_mut()=mmu::Uat::new());
 for p in &G17P_FW_POOLS { while !p.take().is_null() {} }
 UNMAPS.store(0, Ordering::SeqCst); PROOF_MODE.store(0, Ordering::SeqCst); PROOF_CALLS.store(0, Ordering::SeqCst);
 ALLOCS.store(0, Ordering::SeqCst); G17P_FW_POOL_CACHED_BYTES.store(0, Ordering::SeqCst);
 for s in &G17P_FW_POOL_RESERVED {s.store(0,Ordering::SeqCst);}
 FAIL_POOL_NODE.store(false,Ordering::SeqCst);
 for s in &G17P_FW_POOL_STATS { s.store(0, Ordering::SeqCst); }
}
fn knobs(families: u32) { DIAG_KNOBS.store(0x9000 | (families << 27), Ordering::SeqCst); }
fn new_obj(dev: &AsahiDevice, family: u32, size: usize, prefix: usize, wc: bool) -> Result<MappedObject> {
 MappedObject::new_fw_pooled(dev, &default_uat(), family, size, prefix, wc, || MappedObject::plain(size, wc))
}
const DEV: AsahiDevice = AsahiDevice;

#[test] fn disabled_family_is_the_plain_object_and_unmaps_normally() {
 reset(); knobs(0);
 let o = new_obj(&DEV, G17P_FW_FAMILY_DESCRIPTOR, 0x14000, 0x14000, false).unwrap();
 assert_eq!(o.fw_pool, 0); assert_eq!(PROOF_CALLS.load(Ordering::SeqCst), 0);
 drop(o); assert_eq!(UNMAPS.load(Ordering::SeqCst), 1);
}

#[test] fn cached_object_is_pooled_never_unmapped_and_reused_zeroed() {
 reset(); knobs(0xf);
 let mut o = new_obj(&DEV, G17P_FW_FAMILY_TSQ_NODE, 2*UAT_PGSZ, UAT_PGSZ, true).unwrap();
 assert_eq!(o.fw_pool, G17P_FW_FAMILY_TSQ_NODE as u8);
 let iova = o.iova; o.with_bytes_mut(|b| { b.fill(0x77); Ok(()) }).unwrap();
 drop(o);
 assert_eq!(UNMAPS.load(Ordering::SeqCst), 0, "cached mapping unmapped early");
 assert_eq!(g17p_fw_pool_stat(2), 1);
 let o = new_obj(&DEV, G17P_FW_FAMILY_TSQ_NODE, 2*UAT_PGSZ, UAT_PGSZ, true).unwrap();
 assert_eq!(o.iova, iova); assert_eq!(ALLOCS.load(Ordering::SeqCst), 1, "reuse must not allocate");
 assert!(o.bytes.borrow().iter().all(|b| *b == 0), "reused object must be zeroed");
 assert_eq!((g17p_fw_pool_stat(0), g17p_fw_pool_stat(1)), (1, 1));
 // Other families never receive it.
 drop(o);
 let c = new_obj(&DEV, G17P_FW_FAMILY_CONFIG_UPDATE, 2*UAT_PGSZ, 2*UAT_PGSZ, false).unwrap();
 assert_ne!(c.iova, iova); drop(c);
 assert_eq!(UNMAPS.load(Ordering::SeqCst), 0);
 stop_default(); g17p_fw_pool_release_all();
 assert_eq!(UNMAPS.load(Ordering::SeqCst), 2, "release_all unmaps each pooled mapping once");
}

#[test] fn refused_proof_before_mutation_falls_back_uncached() {
 reset(); knobs(0xf); PROOF_MODE.store(1, Ordering::SeqCst);
 let o = new_obj(&DEV, G17P_FW_FAMILY_DESCRIPTOR, 0x14000, 0x14000, false).unwrap();
 assert_eq!(o.fw_pool, 0); assert_eq!(G17P_FW_POOL_CACHED_BYTES.load(Ordering::SeqCst), 0);
 drop(o); assert_eq!(UNMAPS.load(Ordering::SeqCst), 1);
}

#[test] fn failed_proof_after_mutation_is_never_published_reused_or_unmapped() {
 reset(); knobs(0xf); PROOF_MODE.store(2, Ordering::SeqCst);
 assert_eq!(new_obj(&DEV, G17P_FW_FAMILY_CHANNEL_STATE, 7*UAT_PGSZ, 2*UAT_PGSZ, true).err(), Some(EFAULT));
 assert_eq!(UNMAPS.load(Ordering::SeqCst), 0); assert_eq!(g17p_fw_pool_stat(3), 1);
 PROOF_MODE.store(0, Ordering::SeqCst);
 let o = new_obj(&DEV, G17P_FW_FAMILY_CHANNEL_STATE, 7*UAT_PGSZ, 2*UAT_PGSZ, true).unwrap();
 assert_eq!(ALLOCS.load(Ordering::SeqCst), 2, "poisoned object must not be pooled"); drop(o);
}

#[test] fn above_capacity_peaks_fall_back_before_mutation_and_release_everything() {
 for _ in 0..32 {
  reset(); knobs(1);
  let objs:Vec<_>=(0..G17P_FW_POOL_SLOTS+3).map(|_|new_obj(&DEV,1,0x14000,0x14000,false).unwrap()).collect();
  assert_eq!(PROOF_CALLS.load(Ordering::SeqCst),G17P_FW_POOL_SLOTS);
  drop(objs);assert_eq!(UNMAPS.load(Ordering::SeqCst),3);
  assert_eq!(g17p_fw_pool_stat(3),0);
  stop_default();g17p_fw_pool_release_all();assert_eq!(UNMAPS.load(Ordering::SeqCst),G17P_FW_POOL_SLOTS+3);
  assert_eq!(G17P_FW_POOL_CACHED_BYTES.load(Ordering::SeqCst),0);
  assert_eq!(G17P_FW_POOL_RESERVED[0].load(Ordering::SeqCst),0);
 }
}

#[test] fn geometry_mismatch_is_poisoned_not_unmapped() {
 reset(); knobs(0xf);
 drop(new_obj(&DEV, G17P_FW_FAMILY_CONFIG_UPDATE, 2*UAT_PGSZ, 2*UAT_PGSZ, false).unwrap());
 let o = new_obj(&DEV, G17P_FW_FAMILY_CONFIG_UPDATE, 4*UAT_PGSZ, 4*UAT_PGSZ, false).unwrap();
 assert_eq!(o.logical_size, 4*UAT_PGSZ); assert_eq!(UNMAPS.load(Ordering::SeqCst), 0); assert_eq!(g17p_fw_pool_stat(3), 0);
 drop(o); stop_default(); g17p_fw_pool_release_all();
}

#[test] fn cached_byte_cap_falls_back_uncached() {
 reset(); knobs(0xf);
 G17P_FW_POOL_CACHED_BYTES.store(G17P_FW_POOL_CACHED_LIMIT, Ordering::SeqCst);
 let o = new_obj(&DEV, G17P_FW_FAMILY_DESCRIPTOR, 0x14000, 0x14000, false).unwrap();
 assert_eq!(o.fw_pool, 0); assert_eq!(PROOF_CALLS.load(Ordering::SeqCst), 0); drop(o);
}

#[test] fn open_close_cycles_are_bounded_by_peak() {
 reset(); knobs(0xf);
 for _ in 0..10_000 {
  let a = new_obj(&DEV, G17P_FW_FAMILY_DESCRIPTOR, 0x14000, 0x14000, false).unwrap();
  let b = new_obj(&DEV, G17P_FW_FAMILY_DESCRIPTOR, 0x14000, 0x14000, false).unwrap();
  drop(a); drop(b);
 }
 assert_eq!(ALLOCS.load(Ordering::SeqCst), 2, "steady state reuses the peak set");
 assert_eq!(UNMAPS.load(Ordering::SeqCst), 0); assert_eq!(g17p_fw_pool_stat(3), 0);
 stop_default(); g17p_fw_pool_release_all(); assert_eq!(UNMAPS.load(Ordering::SeqCst), 2);
}

#[test] fn pool_slots_never_lose_or_duplicate_entries_under_contention() {
 static P: G17PFwPoolSlots<u64, 8> = G17PFwPoolSlots::new();
 let seen = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
 let hs: Vec<_> = (0..8).map(|t| { let seen = seen.clone(); std::thread::spawn(move || {
  for i in 0..20_000u64 {
   let v = Box::into_raw(Box::new(t * 1_000_000 + i));
   let mut pending = v;
   loop { match P.put(pending) { Ok(()) => break, Err(back) => { assert_eq!(back, v); let x = P.take(); if !x.is_null() { let b = unsafe { Box::from_raw(x) }; assert!(seen.lock().unwrap().insert(*b)); } pending = back; } } }
   let x = P.take(); if !x.is_null() { let b = unsafe { Box::from_raw(x) }; assert!(seen.lock().unwrap().insert(*b)); }
  } }) }).collect();
 for h in hs { h.join().unwrap(); }
 loop { let x = P.take(); if x.is_null() { break; } let b = unsafe { Box::from_raw(x) }; assert!(seen.lock().unwrap().insert(*b)); }
 assert_eq!(seen.lock().unwrap().len(), 8 * 20_000);
 assert_eq!(P.put(std::ptr::null_mut()), Err(std::ptr::null_mut()));
}

#[test] fn family_gate_bits_27_to_30() {
 for m in 0..16u32 { DIAG_KNOBS.store(0x9000 | 0x4000000 | (m << 27), Ordering::SeqCst); assert_eq!(g17_timing_buffer::fw_cached_family_mask(), m); }
 for (f, i) in [(1u32, 0usize), (2, 1), (4, 2), (8, 3)] { assert_eq!(g17p_fw_pool_index(f), Some(i)); }
 for f in [0u32, 3, 5, 16, 0x80] { assert_eq!(g17p_fw_pool_index(f), None); }
}
#[test] fn failed_stop_sweep_does_not_release_unproven_firmware_owner() {
 reset();knobs(0xf);drop(new_obj(&DEV,1,0x14000,0x14000,false).unwrap());
 g17p_fw_pool_release_all();assert_eq!(UNMAPS.load(Ordering::SeqCst),0);
 stop_default();g17p_fw_pool_release_all();assert_eq!(UNMAPS.load(Ordering::SeqCst),1);
}
#[test] fn late_close_after_shutdown_releases_instead_of_refilling() {
 reset();knobs(0xf);let o=new_obj(&DEV,2,2*UAT_PGSZ,UAT_PGSZ,true).unwrap();
 stop_default();g17p_fw_pool_release_all();assert_eq!(UNMAPS.load(Ordering::SeqCst),0);
 drop(o);assert_eq!(UNMAPS.load(Ordering::SeqCst),1);assert!(G17P_FW_POOLS[1].take().is_null());
 assert_eq!(G17P_FW_POOL_CACHED_BYTES.load(Ordering::SeqCst),0);
}
#[test] fn reprobe_never_reuses_previous_uat_mapping_with_identical_geometry() {
 reset();knobs(0xf);let o=new_obj(&DEV,1,0x14000,0x14000,false).unwrap();let old=o.iova;drop(o);
 DEFAULT_UAT.with(|u|*u.borrow_mut()=mmu::Uat::new());
 let n=new_obj(&DEV,1,0x14000,0x14000,false).unwrap();assert_ne!(old,n.iova);
 assert_eq!(UNMAPS.load(Ordering::SeqCst),0,"unproven old owner must remain mapped");
 assert_eq!(g17p_fw_pool_stat(3),0);drop(n);stop_default();g17p_fw_pool_release_all();assert_eq!(UNMAPS.load(Ordering::SeqCst),1);
}
#[test] fn stopping_new_owner_never_frees_old_failed_stop_pool() {
 reset();knobs(0xf);let old_owner=default_uat();drop(new_obj(&DEV,1,0x14000,0x14000,false).unwrap());
 DEFAULT_UAT.with(|u|*u.borrow_mut()=mmu::Uat::new());drop(new_obj(&DEV,2,2*UAT_PGSZ,UAT_PGSZ,true).unwrap());
 stop_default();g17p_fw_pool_release_all();assert_eq!(UNMAPS.load(Ordering::SeqCst),1);
 assert!(!old_owner.inner.firmware_cache_stopped.load(Ordering::SeqCst));
 old_owner.mark_firmware_cache_stopped();g17p_fw_pool_release_all();assert_eq!(UNMAPS.load(Ordering::SeqCst),2);
}
#[test] fn owner_token_pins_identity_and_stop_state_after_uat_handle_drops() {
 reset();knobs(0xf);let ua=default_uat();let o=new_obj(&DEV,1,0x14000,0x14000,false).unwrap();
 let owner=o.mapping.as_ref().unwrap().firmware_cache_owner();
 assert!(owner.matches(&ua));let another=mmu::Uat::new();assert!(!owner.matches(&another));
 ua.mark_firmware_cache_stopped();drop(ua);assert!(owner.stopped());assert!(!owner.matches(&default_uat()));drop(o);
}

#[test] fn allocation_failure_is_uncached_before_any_attribute_mutation() {
 reset();knobs(1);FAIL_POOL_NODE.store(true,Ordering::SeqCst);
 let o=new_obj(&DEV,1,0x14000,0x14000,false).unwrap();
 assert_eq!(o.fw_pool,0);assert_eq!(PROOF_CALLS.load(Ordering::SeqCst),0);
 drop(o);assert_eq!(UNMAPS.load(Ordering::SeqCst),1);
 assert_eq!(G17P_FW_POOL_CACHED_BYTES.load(Ordering::SeqCst),0);
}
#[test] fn poisoned_entries_are_released_at_proven_stop() {
 reset();knobs(1);PROOF_MODE.store(2,Ordering::SeqCst);
 assert!(new_obj(&DEV,1,0x14000,0x14000,false).is_err());
 assert_eq!(UNMAPS.load(Ordering::SeqCst),0);
 stop_default();g17p_fw_pool_release_all();assert_eq!(UNMAPS.load(Ordering::SeqCst),1);
 assert_eq!(G17P_FW_POOL_CACHED_BYTES.load(Ordering::SeqCst),0);
 assert_eq!(G17P_FW_POOL_RESERVED[0].load(Ordering::SeqCst),0);
}
#[test] fn prefix_is_part_of_reuse_geometry() {
 reset();knobs(1);let a=new_obj(&DEV,1,0x14000,0x4000,false).unwrap();let va=a.iova;drop(a);
 let b=new_obj(&DEV,1,0x14000,0x8000,false).unwrap();assert_ne!(b.iova,va);drop(b);
 let a=new_obj(&DEV,1,0x14000,0x4000,false).unwrap();assert_eq!(a.iova,va);drop(a);
 stop_default();g17p_fw_pool_release_all();assert_eq!(UNMAPS.load(Ordering::SeqCst),2);
}
#[test] fn sweep_observing_live_owner_cannot_restore_after_that_owners_final_sweep() {
 reset();knobs(1);let owner=default_uat();drop(new_obj(&DEV,1,0x14000,0x14000,false).unwrap());
 let entered=std::sync::Arc::new(std::sync::Barrier::new(2));
 let resume=std::sync::Arc::new(std::sync::Barrier::new(2));
 *STOP_READ_HOOK.lock().unwrap()=Some((entered.clone(),resume.clone()));
 let first=std::thread::spawn(||g17p_fw_pool_release_all());
 entered.wait();owner.mark_firmware_cache_stopped();
 let second=std::thread::spawn(||g17p_fw_pool_release_all());
 resume.wait();first.join().unwrap();second.join().unwrap();
 // No compensating cleanup sweep: the actual racing operations must finish it.
 assert_eq!(UNMAPS.load(Ordering::SeqCst),1);
 assert_eq!(G17P_FW_POOL_CACHED_BYTES.load(Ordering::SeqCst),0);
 assert_eq!(G17P_FW_POOL_RESERVED[0].load(Ordering::SeqCst),0);
}
#[test] fn concurrent_final_stop_and_owner_drops_release_exactly_once() {
 for _ in 0..128 {
  reset();knobs(1);let owner=default_uat();
  let objects:Vec<_>=(0..16).map(|_|new_obj(&DEV,1,0x14000,0x14000,false).unwrap()).collect();
  let barrier=std::sync::Arc::new(std::sync::Barrier::new(2));let worker=barrier.clone();
  let thread=std::thread::spawn(move||{worker.wait();drop(objects);});
  barrier.wait();owner.mark_firmware_cache_stopped();g17p_fw_pool_release_all();thread.join().unwrap();
  assert_eq!(UNMAPS.load(Ordering::SeqCst),16);
  assert_eq!(G17P_FW_POOL_CACHED_BYTES.load(Ordering::SeqCst),0);assert_eq!(g17p_fw_pool_stat(3),0);
 }
}

'''
source = source.replace('use crate::', 'use ').replace('crate::g17_timing_buffer::', 'g17_timing_buffer::')

# mmu harness for the actual PTE-proof method.
mmu_src = r'''
#![allow(dead_code)]
use std::{cell::{RefCell,RefMut},rc::Rc,ops::Range,sync::atomic::{AtomicUsize,Ordering,fence}};
type Result<T>=std::result::Result<T,i32>;
const EINVAL:i32=22; const EFAULT:i32=14; const UAT_PGSZ:usize=16384;
const PROT_GPU_FW_PRIV_RW:u64=0;
static FLUSH:AtomicUsize=AtomicUsize::new(0); static VALIDATIONS:AtomicUsize=AtomicUsize::new(0);
static DROPS:AtomicUsize=AtomicUsize::new(0);
mod mem {pub fn tlbi_all(){super::FLUSH.fetch_add(1,super::Ordering::Relaxed);}pub fn sync(){}}
mod pgtable {#[derive(Default,Clone,Copy)]pub struct MappedRange{pub pte:u64}}
struct PageTable{ptes:Vec<u64>,fail_remap:bool,corrupt_tail:bool}
impl PageTable{
fn collect_mapped_ranges(&mut self,r:Range<u64>,out:&mut [pgtable::MappedRange])->Result<(usize,bool)>{out[0].pte=self.ptes[((r.start-0x10000)/UAT_PGSZ as u64)as usize];Ok((1,false))}
fn reprot_pages(&mut self,r:Range<u64>,_:u64)->Result<()>{assert_eq!(r.start,0x10000);for i in 0..((r.end-r.start)/UAT_PGSZ as u64)as usize{self.ptes[i]&=!8;}if self.corrupt_tail{*self.ptes.last_mut().unwrap()^=4;}if self.fail_remap{Err(EFAULT)}else{Ok(())}}
}
struct VmInner{is_kernel:bool,page_table:PageTable}
impl VmInner{fn note_mapping_validation_change(&self,r:Range<u64>){assert!(r.end>r.start);VALIDATIONS.fetch_add(1,Ordering::Relaxed);}}
struct Owner(Rc<RefCell<VmInner>>);
impl Owner{fn exec_lock(&self,_:Option<()>,_:bool)->Result<RefMut<'_,VmInner>>{Ok(self.0.borrow_mut())}}
struct UatInner{t8140:bool} struct Inner{uat_inner:UatInner,owner:Owner,size:usize}
struct KernelMapping(Inner);
impl Drop for KernelMapping{fn drop(&mut self){DROPS.fetch_add(1,Ordering::Relaxed);}}
impl KernelMapping{fn size(&self)->usize{self.0.size}fn iova(&self)->u64{0x10000}
''' + prefix_method + r'''
}
fn main(){
 for pages in [2usize,7,23,32] { for prefix_pages in 0..=pages { for (fail,corrupt) in [(false,false),(true,false),(false,true)] {
  let initial:Vec<_>=(0..pages).map(|i|0xc000001234044b+((i as u64)<<14)).collect();
  let owner=Rc::new(RefCell::new(VmInner{is_kernel:true,page_table:PageTable{ptes:initial.clone(),fail_remap:fail,corrupt_tail:corrupt}}));
  let mapping=KernelMapping(Inner{uat_inner:UatInner{t8140:true},owner:Owner(owner.clone()),size:pages*UAT_PGSZ});
  let before=VALIDATIONS.load(Ordering::Relaxed);
  let (r,mutated)=unsafe{mapping.set_fw_cached_prefix(prefix_pages*UAT_PGSZ)};
  if prefix_pages==0 {
   assert!(r.is_err()&&!mutated); assert_eq!(before,VALIDATIONS.load(Ordering::Relaxed));
   assert_eq!(owner.borrow().page_table.ptes,initial);
  } else if !fail&&!corrupt {
   assert!(r.is_ok()&&mutated);
   for i in 0..pages{assert_eq!(owner.borrow().page_table.ptes[i],initial[i]^if i<prefix_pages{8}else{0});}
  } else { assert!(r.is_err()&&mutated); }
  drop(mapping);
 }}}
 assert!(DROPS.load(Ordering::Relaxed)>0, "method must not consume/forget the mapping");
 for (size,prefix) in [(0,0),(32768,32769),(32768,1),(33*UAT_PGSZ,UAT_PGSZ)] {
  let owner=Rc::new(RefCell::new(VmInner{is_kernel:true,page_table:PageTable{ptes:vec![0x44b;33],fail_remap:false,corrupt_tail:false}}));
  let mapping=KernelMapping(Inner{uat_inner:UatInner{t8140:true},owner:Owner(owner.clone()),size});
  let (r,mutated)=unsafe{mapping.set_fw_cached_prefix(prefix)}; assert!(r.is_err()&&!mutated);
  assert!(owner.borrow().page_table.ptes.iter().all(|p|*p==0x44b));
 }
 let owner=Rc::new(RefCell::new(VmInner{is_kernel:false,page_table:PageTable{ptes:vec![0x44b;2],fail_remap:false,corrupt_tail:false}}));
 let mapping=KernelMapping(Inner{uat_inner:UatInner{t8140:true},owner:Owner(owner),size:2*UAT_PGSZ});
 let (r,mutated)=unsafe{mapping.set_fw_cached_prefix(UAT_PGSZ)}; assert!(r.is_err()&&!mutated);
 println!("PASS set_fw_cached_prefix: zero/oversize/user-VM refused unmutated; prefixes flip only bit3; failures report mutation; mapping not consumed");
}
'''
with tempfile.TemporaryDirectory(prefix='g17-fw-pool-') as t:
    t = Path(t)
    (t / 'pool.rs').write_text(source)
    subprocess.run(['rustc', '--edition=2021', '-Awarnings', '--test', str(t / 'pool.rs'), '-o', str(t / 'pool')], check=True)
    subprocess.run([str(t / 'pool'), '--test-threads=1', '--quiet'], check=True)
    (t / 'mmu.rs').write_text(mmu_src)
    subprocess.run(['rustc', '--edition=2021', '-Awarnings', str(t / 'mmu.rs'), '-o', str(t / 'mmu')], check=True)
    subprocess.run([str(t / 'mmu')], check=True)
print('PASS v90 pooled firmware-cached lifecycle (production pool/Drop/new_fw_pooled/release_all + PTE proof)')
