from pathlib import Path
import subprocess,tempfile
p=Path(__file__).resolve().parent
r=p.parent/'mmu.rs'
s=r.read_text()
a=s.index('struct VmDriverMappings {');b=s.index('/// Slot data for a',a)
structs=s[a:b]
def function(source, marker):
    start=source.index(marker);pos=source.index('{',start)+1;depth=1
    while depth:
        depth+=(source[pos]=='{')-(source[pos]=='}');pos+=1
    return source[start:pos]
methods='\n'.join(function(s, marker) for marker in [
    '    pub(crate) fn install_driver_mappings(',
    '    pub(crate) fn append_driver_mappings(',
    '    pub(crate) fn render_pool_mappings_match(',
    '    pub(crate) fn install_render_pool_mappings(',
    '    pub(crate) fn render_pool_tvb_blocks(',
    '    pub(crate) fn append_render_pool_mappings(',
    '    pub(crate) fn clear_render_pool_mappings(',
    '    pub(crate) fn clear_render_pool_mappings_if_owner(',
    '    pub(crate) fn compute_pool_mappings_match(',
    '    pub(crate) fn install_compute_pool_mappings(',
    '    pub(crate) fn clear_compute_pool_mappings(',
    '    pub(crate) fn driver_range_overlaps(',
    '    pub(crate) fn unmap_user_ranges(',
    '    fn unmap_user_ranges_now(',
])
# Kernel KVec::reserve carries an allocation flag and returns Result; adapt
# that spelling to std::Vec while executing the otherwise exact methods.
methods=methods.replace('.reserve(added, GFP_KERNEL)?;', '.try_reserve(added).map_err(|_| EOVERFLOW)?;')
preamble=r'''
#![allow(dead_code)]
use std::ops::{Range,Deref,DerefMut};
use std::pin::Pin;
// Kernel prelude exports Init::chain for every type; model that ambiguity.
trait Init:Sized{fn chain(self,_next:impl Sized){}}
impl<T>Init for T{}
use std::sync::{Arc,Mutex as StdMutex,MutexGuard};
use std::sync::atomic::{AtomicBool,AtomicUsize,Ordering};
type KVec<T> = Vec<T>;
type Result<T=()> = std::result::Result<T,i32>;
const EINVAL:i32=22;const EBUSY:i32=16;const EIO:i32=5;const EOVERFLOW:i32=75;
macro_rules! no_debug {($x:ty)=>{}}
trait RangeExt {fn overlaps(&self,other:Range<u64>)->bool;}
impl RangeExt for Range<u64>{fn overlaps(&self,other:Range<u64>)->bool{self.start<other.end&&other.start<self.end}}
struct TestMutex<T>{inner:StdMutex<T>,held:Arc<AtomicBool>}
struct TestGuard<'a,T>{guard:MutexGuard<'a,T>,held:&'a AtomicBool}
impl<T> TestMutex<T>{fn lock(&self)->TestGuard<'_,T>{let guard=self.inner.lock().unwrap();assert!(!self.held.swap(true,Ordering::SeqCst));TestGuard{guard,held:&self.held}}}
// Kernel guards expose an inherent pinned as_mut; Option::as_mut must be explicit.
impl<T:Unpin>TestGuard<'_,T>{fn as_mut(&mut self)->Pin<&mut T>{Pin::new(&mut *self.guard)}}
impl<T>Deref for TestGuard<'_,T>{type Target=T;fn deref(&self)->&T{&self.guard}}
impl<T>DerefMut for TestGuard<'_,T>{fn deref_mut(&mut self)->&mut T{&mut self.guard}}
impl<T>Drop for TestGuard<'_,T>{fn drop(&mut self){assert!(self.held.swap(false,Ordering::SeqCst));}}
struct KernelMapping{range:Range<u64>,held:Arc<AtomicBool>,drops:Arc<AtomicUsize>}
impl KernelMapping{fn iova_range(&self)->Range<u64>{self.range.clone()}}
impl Drop for KernelMapping{fn drop(&mut self){assert!(!self.held.load(Ordering::SeqCst),"mapping destroyed under driver_mappings lock");self.drops.fetch_add(1,Ordering::SeqCst);}}
'''
vm=r'''
struct JobLifetime{active:usize,closed:bool,close_ranges:Option<(Range<u64>,Range<u64>)>}
struct MappingGateState{blocked:bool,commits:usize}
struct Idle;
impl Idle{fn wait<T>(&self,_guard:&mut TestGuard<'_,T>){unreachable!()}fn notify_all(&self){}}
struct MappingGate{state:TestMutex<MappingGateState>,idle:Idle}
struct Vm{job_lifetime:TestMutex<JobLifetime>,mapping_gate:MappingGate,driver_mappings:TestMutex<Option<VmDriverMappings>>,unmapped:StdMutex<Vec<Range<u64>>>}
impl Vm{
fn new()->Self{Self{job_lifetime:TestMutex{inner:StdMutex::new(JobLifetime{active:0,closed:false,close_ranges:None}),held:Arc::new(AtomicBool::new(false))},mapping_gate:MappingGate{state:TestMutex{inner:StdMutex::new(MappingGateState{blocked:false,commits:0}),held:Arc::new(AtomicBool::new(false))},idle:Idle},driver_mappings:TestMutex{inner:StdMutex::new(None),held:Arc::new(AtomicBool::new(false))},unmapped:StdMutex::new(Vec::new())}}
fn mapping(&self,range:Range<u64>,drops:&Arc<AtomicUsize>)->KernelMapping{KernelMapping{range,held:self.driver_mappings.held.clone(),drops:drops.clone()}}
fn unmap_range(&self,start:u64,size:u64)->Result{self.unmapped.lock().unwrap().push(start..start+size);Ok(())}
'''
tests=r'''
fn main(){
// A delayed old-graph teardown must not unmap a newly installed owner.
let vm=Vm::new();let drops=Arc::new(AtomicUsize::new(0));
vm.install_driver_mappings(vec![],vec![]).unwrap();
vm.install_render_pool_mappings(7,70,1,vec![vm.mapping(16..32,&drops)]).unwrap();
vm.clear_render_pool_mappings_if_owner(7,70);assert_eq!(drops.load(Ordering::SeqCst),1);
vm.install_render_pool_mappings(7,71,1,vec![vm.mapping(16..32,&drops)]).unwrap();
for (slot,id) in [(7,70),(7,0),(6,71),(255,71)] {vm.clear_render_pool_mappings_if_owner(slot,id);}
assert!(vm.render_pool_mappings_match(7,71));assert_eq!(drops.load(Ordering::SeqCst),1);
vm.clear_render_pool_mappings_if_owner(7,71);assert_eq!(drops.load(Ordering::SeqCst),2);
vm.clear_render_pool_mappings_if_owner(7,71);assert_eq!(drops.load(Ordering::SeqCst),2);
drop(vm);

let drops=Arc::new(AtomicUsize::new(0));let vm=Vm::new();
assert!(!vm.render_pool_mappings_match(0,1));assert!(!vm.render_pool_mappings_match(1,1));vm.clear_render_pool_mappings(0);
assert_eq!(vm.install_render_pool_mappings(2,1,1,vec![vm.mapping(96..112,&drops)]),Err(EINVAL));assert_eq!(drops.load(Ordering::SeqCst),1);
assert_eq!(vm.install_driver_mappings(vec![vm.mapping(16..32,&drops)],vec![192..224]),Ok(()));
assert_eq!(vm.install_render_pool_mappings(0,0,1,vec![vm.mapping(96..112,&drops)]),Err(EINVAL));assert_eq!(drops.load(Ordering::SeqCst),2);
assert_eq!(vm.install_render_pool_mappings(0,7,2,vec![vm.mapping(96..112,&drops),vm.mapping(160..176,&drops)]),Ok(()));
assert_eq!(vm.install_render_pool_mappings(1,8,1,vec![vm.mapping(128..144,&drops)]),Ok(()));
assert!(vm.render_pool_mappings_match(0,7));assert!(vm.render_pool_mappings_match(1,8));assert!(!vm.render_pool_mappings_match(0,8));assert!(!vm.render_pool_mappings_match(1,7));
assert_eq!(vm.render_pool_tvb_blocks(0,7),Some(2));assert_eq!(vm.render_pool_tvb_blocks(1,8),Some(1));
assert_eq!(vm.install_render_pool_mappings(0,9,1,vec![vm.mapping(48..64,&drops)]),Err(EBUSY));
assert_eq!(vm.install_render_pool_mappings(1,9,1,vec![vm.mapping(64..80,&drops)]),Err(EBUSY));assert_eq!(drops.load(Ordering::SeqCst),4);
assert!(vm.driver_range_overlaps(20..21));assert!(vm.driver_range_overlaps(100..101));assert!(vm.driver_range_overlaps(136..137));assert!(vm.driver_range_overlaps(168..169));assert!(vm.driver_range_overlaps(200..201));assert!(!vm.driver_range_overlaps(32..64));assert!(!vm.driver_range_overlaps(80..96));
assert_eq!(vm.unmap_user_ranges(0..256,64..80),Ok(()));
assert_eq!(*vm.unmapped.lock().unwrap(),vec![0..16,32..64,80..96,112..128,144..160,176..192,224..256]);
vm.clear_render_pool_mappings(0);assert_eq!(drops.load(Ordering::SeqCst),6);assert!(!vm.render_pool_mappings_match(0,7));assert!(vm.render_pool_mappings_match(1,8));assert!(!vm.driver_range_overlaps(100..101));assert!(vm.driver_range_overlaps(136..137));
vm.clear_render_pool_mappings(0);assert_eq!(drops.load(Ordering::SeqCst),6);
vm.unmapped.lock().unwrap().clear();assert_eq!(vm.unmap_user_ranges(0..256,64..80),Ok(()));assert_eq!(*vm.unmapped.lock().unwrap(),vec![0..16,32..64,80..128,144..192,224..256]);
vm.clear_render_pool_mappings(1);assert_eq!(drops.load(Ordering::SeqCst),7);
assert_eq!(vm.install_render_pool_mappings(1,9,1,vec![vm.mapping(128..144,&drops)]),Ok(()));assert!(vm.render_pool_mappings_match(1,9));
drop(vm);assert_eq!(drops.load(Ordering::SeqCst),9);
let vm=Vm::new();let drops=Arc::new(AtomicUsize::new(0));
vm.install_driver_mappings(vec![],vec![]).unwrap();
vm.install_render_pool_mappings(0,10,1,vec![vm.mapping(16..32,&drops)]).unwrap();
vm.install_compute_pool_mappings(20,vec![vm.mapping(64..96,&drops)]).unwrap();
assert!(vm.compute_pool_mappings_match(20));assert!(!vm.compute_pool_mappings_match(21));
assert!(vm.driver_range_overlaps(80..81));
vm.unmap_user_ranges(0..128,128..128).unwrap();
assert_eq!(*vm.unmapped.lock().unwrap(),vec![0..16,32..64,96..128]);
vm.clear_compute_pool_mappings();assert!(vm.render_pool_mappings_match(0,10));
assert_eq!(drops.load(Ordering::SeqCst),1);vm.clear_compute_pool_mappings();
drop(vm);assert_eq!(drops.load(Ordering::SeqCst),2);
let stress=Vm::new();let stress_drops=Arc::new(AtomicUsize::new(0));
stress.install_driver_mappings(vec![],vec![]).unwrap();
for generation in 1..=4096u64{
 let pool0=generation*2;let pool1=pool0+1;
 assert_eq!(stress.install_render_pool_mappings(0,pool0,1,vec![stress.mapping(16..32,&stress_drops)]),Ok(()));
 assert_eq!(stress.install_render_pool_mappings(1,pool1,1,vec![stress.mapping(64..80,&stress_drops)]),Ok(()));
 assert_eq!(stress.render_pool_tvb_blocks(0,pool0),Some(1));assert_eq!(stress.render_pool_tvb_blocks(1,pool1),Some(1));
 // Failed prepublication replacements own only their temporary mappings.
 assert_eq!(stress.install_render_pool_mappings(0,pool0+2,1,vec![stress.mapping(32..48,&stress_drops)]),Err(EBUSY));
 assert_eq!(stress.install_render_pool_mappings(1,pool1+2,1,vec![stress.mapping(80..96,&stress_drops)]),Err(EBUSY));
 assert_eq!(stress.append_render_pool_mappings(0,pool0,1,2,vec![stress.mapping(32..48,&stress_drops)]),Ok(()));
 stress.clear_render_pool_mappings(0);stress.clear_render_pool_mappings(0);
 assert!(!stress.render_pool_mappings_match(0,pool0));assert!(stress.render_pool_mappings_match(1,pool1));
 stress.clear_render_pool_mappings(1);assert!(!stress.render_pool_mappings_match(1,pool1));
}
assert_eq!(stress_drops.load(Ordering::SeqCst),20480);
println!("PASS actual mmu cache source: 4096 dual-slot coexist/install/reject/grow/clear cycles, mapping drops outside lock, fixed+cached overlap and exact user-unmap exclusions");
}
'''
code=preamble+structs+vm+methods+'}\n'+tests
with tempfile.TemporaryDirectory() as d:
    source=Path(d)/'actual_source_cache_test.rs'
    executable=Path(d)/'test-cache'
    source.write_text(code)
    subprocess.run(['rustc','--edition=2021','-C','opt-level=2',str(source),'-o',str(executable)],check=True)
    subprocess.run([str(executable)],check=True)
