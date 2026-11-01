#!/usr/bin/env python3
"""Compile actual Scene/scratch ownership; reproduce the old wrap alias."""
from pathlib import Path
import subprocess
import sys
import tempfile

source = (Path(__file__).resolve().parents[1] / 'g17_resources.rs').read_text()


def item(marker):
    start = source.index(marker)
    end = source.index('{', start) + 1
    depth = 1
    while depth:
        depth += (source[end] == '{') - (source[end] == '}')
        end += 1
    return source[start:end]


names = ['G17P_PM_SCENE_COUNT', 'G17P_PM_SELECTED_SCENE', 'G17P_PM_SCENE_SCRATCH_SLOT_COUNT']
constants = '\n'.join(next(line for line in source.splitlines() if line.startswith('const ' + n + ':')) for n in names)
implementation = item('impl G17PSceneOwnership {')
if '--negative-control' in sys.argv:
    implementation = implementation.replace(item('    fn select(&self, ordinal: u64)'),
        'fn select(&self, ordinal: u64) -> Option<usize> { g17p_render_scene_index(0, ordinal) }')
program = constants + '\n' + item('fn g17p_render_scene_index(') + '\n'
program += item('struct G17PSceneOwnership {') + '\n' + implementation
program += r'''
#[test]fn old_ordinal_selector_aliases_scratch_inside_sixteen_live_commands(){
 let a=g17p_render_scene_index(0,71).unwrap();let b=g17p_render_scene_index(0,79).unwrap();
 assert_eq!((a,b),(72,0));
 assert_eq!(a%G17P_PM_SCENE_SCRATCH_SLOT_COUNT,b%G17P_PM_SCENE_SCRATCH_SLOT_COUNT);
}
#[test]fn full_depth_wraps_preserve_all_scene_and_scratch_ownership(){
 for depth in [16,32] {
  let mut ring=G17PSceneOwnership::new();let mut live=std::collections::VecDeque::new();
  let mut seen=std::collections::BTreeSet::new();
  for ordinal in 0..20000 {
   if live.len()==depth {let (old,_)=live.pop_front().unwrap();assert!(ring.release(old));}
   let scene=ring.select(ordinal).expect("production depth must fit without waiting");
   assert!(live.iter().all(|(_,other)|*other!=scene && other%G17P_PM_SCENE_SCRATCH_SLOT_COUNT!=scene%G17P_PM_SCENE_SCRATCH_SLOT_COUNT),"alias at ordinal {ordinal}");
   assert!(ring.commit(ordinal,scene));live.push_back((ordinal,scene));seen.insert(scene);
  }
  assert_eq!(live.len(),depth,"full production depth is retained");
  // Under persistent pressure a busy scratch alias can legitimately cause
  // a particular Scene to be skipped on every lap. Once those owners retire,
  // every Scene remains selectable; none was removed from the native table.
  while let Some((old,_))=live.pop_front(){assert!(ring.release(old));}
  for ordinal in 20000..20200 {
   let scene=ring.select(ordinal).unwrap();assert!(ring.commit(ordinal,scene));
   seen.insert(scene);assert!(ring.release(ordinal));
  }
  assert_eq!(seen.len(),G17P_PM_SCENE_COUNT,"all native Scenes remain usable");
 }
}
#[test]fn exact_retirement_out_of_order_and_exhaustion(){
 let mut ring=G17PSceneOwnership::new();let mut live=Vec::new();
 for ordinal in 100..100+G17P_PM_SCENE_SCRATCH_SLOT_COUNT as u64 {
  let scene=ring.select(ordinal).unwrap();assert!(ring.commit(ordinal,scene));live.push((ordinal,scene));
 }
 assert_eq!(ring.select(999),None);assert!(!ring.release(999));
 assert!(!ring.commit(999,0));
 for (ordinal,scene) in live.into_iter().rev(){
  assert_eq!(ring.select(ordinal),Some(scene));assert!(ring.commit(ordinal,scene));
  assert!(ring.release(ordinal));assert!(!ring.release(ordinal));
 }
 assert!(ring.select(u64::MAX).is_some());
}
#[test]fn failed_preparation_does_not_consume_a_slot(){
 let mut ring=G17PSceneOwnership::new();
 assert_eq!(ring.select(195),Some(G17P_PM_SELECTED_SCENE));
 for ordinal in 195..300 {assert_eq!(ring.select(ordinal),Some(G17P_PM_SELECTED_SCENE));}
 assert!(ring.commit(195,G17P_PM_SELECTED_SCENE));
 assert_eq!(ring.select(195),Some(G17P_PM_SELECTED_SCENE));
 assert_eq!(ring.select(196),Some(2));
 assert!(!ring.commit(196,1));assert_eq!(ring.select(196),Some(2));
}
'''
# The reservation and release are coupled to the actual descriptor lifetime.
retire = item('    pub(crate) fn retire_command_generation(')
restage = item('    pub(crate) fn restage_command(')
assert 'pm.scene_ownership.release(submission_ordinal)' in retire
assert restage.index('pm.scene_ownership.commit(submission_ordinal, pm.selected_scene)') < restage.index('self.render_command_ordinals[generation] = Some(submission_ordinal)')
assert 'self.scene_ownership.select(submission_ordinal)' in item('    fn prepare_retained_scene(')
with tempfile.TemporaryDirectory() as directory:
    src = Path(directory) / 'scenes.rs'
    exe = Path(directory) / 'scenes'
    src.write_text(program)
    subprocess.run(['rustc', '--edition=2021', '--test', str(src), '-o', str(exe)], check=True)
    args = [str(exe), '--test-threads=1']
    if '--negative-control' in sys.argv:
        args.append('full_depth_wraps_preserve_all_scene_and_scratch_ownership')
    subprocess.run(args, check=True)
print('PASS actual Scene/scratch allocator at full 16/32 depth and exact retirement')
