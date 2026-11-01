"""Shared adapters for extracted VM-loss render ownership methods."""
from pathlib import Path
from test_g17_compute_scs_encoding import block
ROOT = Path(__file__).resolve().parents[1]

def fence_state_source():
    s = (ROOT / "g17_completion.rs").read_text()
    return ("#[derive(Default)]\n" + block(s, "pub(crate) struct RenderFenceState {")
            + block(s, "impl RenderFenceState {"))

def retention_method(source):
    s = (source / "g17_live_boot.rs").read_text()
    # On the negative control the fanout never calls this new helper. Keep it
    # available so the same harness can compile both implementations.
    if "    fn quarantine_failed_render_completion(" not in s:
        s = (ROOT / "g17_live_boot.rs").read_text()
    return block(s, "    fn quarantine_failed_render_completion(")

KVEC = r'''
const GFP_KERNEL:u32=0;
struct KVec<T>(Vec<T>);
impl<T> KVec<T> {
 fn new()->Self{Self(Vec::new())}
 fn reserve(&mut self,n:usize,_:u32)->std::result::Result<(),()>{self.0.reserve(n);Ok(())}
 fn with_capacity(n:usize)->Self{Self(Vec::with_capacity(n))}
 fn push_within_capacity(&mut self,v:T)->std::result::Result<(),(T,)> {
  if self.0.len()<self.0.capacity(){self.0.push(v);Ok(())}else{Err((v,))}
 }
 fn clear(&mut self){self.0.clear();}
 fn swap_remove(&mut self,n:usize)->T{self.0.swap_remove(n)}
}
impl<T> std::ops::Deref for KVec<T>{type Target=[T];fn deref(&self)->&[T]{&self.0}}
impl<T> std::ops::DerefMut for KVec<T>{fn deref_mut(&mut self)->&mut[T]{&mut self.0}}
impl<'a,T> IntoIterator for &'a KVec<T>{type Item=&'a T;type IntoIter=std::slice::Iter<'a,T>;fn into_iter(self)->Self::IntoIter{self.0.iter()}}
impl<T> IntoIterator for KVec<T>{type Item=T;type IntoIter=std::vec::IntoIter<T>;fn into_iter(self)->Self::IntoIter{self.0.into_iter()}}
'''
