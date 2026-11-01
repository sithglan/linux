"""In-memory TSQ backing for production lease tests; no firmware simulation."""
def tsq_backing():
    return r'''
struct TsqLock(std::sync::Mutex<TestTsq>);
impl TsqLock {
 fn new()->Self {Self(std::sync::Mutex::new(TestTsq {
  words:[None;63], counts:[0;63], nodes:[[0;256];63],
  render_slots:crate::g17_work_state::RenderTsqSlots::new(),
 }))}
 fn lock(&self)->std::sync::MutexGuard<'_,TestTsq>{self.0.lock().unwrap()}
}
struct TestTsq {
 words:[Option<u32>;63], counts:[u32;63], nodes:[[u8;256];63],
 render_slots:crate::g17_work_state::RenderTsqSlots,
}
impl TestTsq {
 fn initialize(&mut self,word:u32)->Result {
  let slot=(word&63) as usize;
  if slot>=63 || !crate::g17_work_state::initialize_tsq_node(
   &mut self.nodes[slot], 0x14000+(slot*4) as u64, word,80) {return Err(EINVAL)}
  self.words[slot]=Some(word);self.counts[slot]=0;Ok(())
 }
 fn change_count(&mut self,word:u32,kicks:u32,add:bool)->Result<u32>{
  let slot=(word&63) as usize;
  if self.words.get(slot)!=Some(&Some(word)){return Err(EINVAL)}
  let next=if add {self.counts[slot].checked_add(kicks)}else{self.counts[slot].checked_sub(kicks)}.ok_or(EOVERFLOW)?;
  self.counts[slot]=next;Ok(next)
 }
 fn counters(&self,word:u32)->Result<[u32;2]>{
  let slot=(word&63) as usize;
  if self.words.get(slot)!=Some(&Some(word)){return Err(EINVAL)}
  Ok([self.counts[slot],0])
 }
}
'''
