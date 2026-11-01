#!/usr/bin/env python3
"""Run the production wake predicate against both RTKit event roles."""
from pathlib import Path
import subprocess,tempfile
s=(Path(__file__).resolve().parents[1]/'g17_live_boot.rs').read_text();a=s.index('    pub(crate) fn primary_firmware_event_pending(');e=s.index('{',a)+1;d=1
while d:d+=(s[e]=='{')-(s[e]=='}');e+=1
method=s[a:e]
source=r'''
use std::sync::{Arc,atomic::{AtomicU32,Ordering}};
struct State{firmware_event_notifications:AtomicU32}
struct Runtime{gfx_state:Option<Arc<State>>,gfx1_state:Option<Arc<State>>,queue_setup_pending:bool,control_backpressured:bool,release_pending:bool,crashed:bool}
impl Runtime{fn is_crashed(&self)->bool{self.crashed}fn primary_device_control_backpressured(&self)->bool{!self.crashed&&(self.control_backpressured||self.release_pending)}
'''+method+r'''
}
fn runtime()->Runtime{Runtime{gfx_state:Some(Arc::new(State{firmware_event_notifications:AtomicU32::new(0)})),gfx1_state:Some(Arc::new(State{firmware_event_notifications:AtomicU32::new(0)})),queue_setup_pending:false,control_backpressured:false,release_pending:false,crashed:false}}
#[test]fn secondary_only_notification_requeues_shared_worker(){let r=runtime();assert!(!r.primary_firmware_event_pending());r.gfx1_state.as_ref().unwrap().firmware_event_notifications.store(128,Ordering::Release);assert!(r.primary_firmware_event_pending());}
#[test]fn notification_after_consumer_snapshot_is_not_lost(){let r=runtime();let secondary=r.gfx1_state.as_ref().unwrap();secondary.firmware_event_notifications.store(1,Ordering::Release);assert_eq!(secondary.firmware_event_notifications.swap(0,Ordering::AcqRel),1);assert!(!r.primary_firmware_event_pending());secondary.firmware_event_notifications.fetch_add(1,Ordering::Release);assert!(r.primary_firmware_event_pending());}
#[test]fn active_job_does_not_create_a_polling_wake(){let mut r=runtime();assert!(!r.primary_firmware_event_pending());r.gfx_state.as_ref().unwrap().firmware_event_notifications.store(1,Ordering::Release);assert!(r.primary_firmware_event_pending());r.crashed=true;assert!(!r.primary_firmware_event_pending());}
#[test]fn consumed_event_with_deferred_reply_keeps_worker_pending(){let mut r=runtime();r.control_backpressured=true;assert!(r.primary_firmware_event_pending());r.crashed=true;assert!(!r.primary_firmware_event_pending());}
#[test]fn deferred_flist_release_keeps_worker_pending(){let mut r=runtime();r.release_pending=true;assert!(r.primary_firmware_event_pending());r.crashed=true;assert!(!r.primary_firmware_event_pending());}
'''
with tempfile.TemporaryDirectory() as t:
 f=Path(t)/'test.rs';f.write_text(source);exe=Path(t)/'test';subprocess.run(['rustc','--edition=2021','--test',str(f),'-o',str(exe)],check=True);subprocess.run([str(exe)],check=True)
print('PASS actual wake predicate: both roles, post-snapshot notification, deferred control reply, and crash stop')
