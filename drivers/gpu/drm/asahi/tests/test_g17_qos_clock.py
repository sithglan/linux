#!/usr/bin/env python3
"""Exercise the production QoS clock conversion against an independent wide oracle."""
from pathlib import Path
import subprocess,tempfile
from test_g17_compute_scs_encoding import block
source=(Path(__file__).resolve().parents[1]/'g17_resources.rs').read_text()
body=block(source,'fn g17p_qos_time_from_host(')
code='type Result<T> = std::result::Result<T,i32>; const EINVAL:i32=22; const EOVERFLOW:i32=75;\n'+body+r'''
#[test]fn neo_and_native_counter_domains(){
 assert_eq!(g17p_qos_time_from_host(1_000_000_000,1_000_000_000),Ok(24_000_000));
 assert_eq!(g17p_qos_time_from_host(24_000_000,24_000_000),Ok(24_000_000));
 for counter in [0,1,41,42,999_999_999,138_745_295_567,146_988_123_269,u64::MAX] {
  for hz in [1u64,24_000_000,1_000_000_000,u32::MAX as u64] {
   let expected=(counter as u128)*24_000_000/(hz as u128);
   let expected=if expected>u64::MAX as u128 {Err(EOVERFLOW)} else {Ok(expected as u64)};
   assert_eq!(g17p_qos_time_from_host(counter,hz),expected);
  }
 }
 // A raw 1 GHz second must not be accepted as a 24 MHz second.
 assert_ne!(g17p_qos_time_from_host(1_000_000_000,1_000_000_000),Ok(1_000_000_000));
}
#[test]fn invalid_rates_and_no_multiply_before_divide_overflow(){
 assert_eq!(g17p_qos_time_from_host(1,0),Err(EINVAL));
 assert_eq!(g17p_qos_time_from_host(1,u32::MAX as u64+1),Err(EINVAL));
 assert_eq!(g17p_qos_time_from_host(u64::MAX,24_000_000),Ok(u64::MAX));
 assert_eq!(g17p_qos_time_from_host(u64::MAX,1),Err(EOVERFLOW));
}
'''
with tempfile.TemporaryDirectory() as d:
 exe=str(Path(d)/'test')
 subprocess.run(['rustc','--edition=2021','--test','-','-o',exe],input=code,text=True,check=True)
 subprocess.run([exe],check=True)
