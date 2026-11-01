#!/usr/bin/env python3
"""Actual high-VA range selector with drm_mm guard-node allocation model."""
from pathlib import Path
import re,subprocess,tempfile
p=Path(__file__).resolve().parents[1];s=(p/'g17_resources.rs').read_text()
constants='\n'.join(re.search(r'const '+n+r': [^;]+;',s)[0] for n in ['G17P_BOOTSTRAP_TA_STATUS_VA','G17P_BOOTSTRAP_3D_STATUS_VA'])
a=s.index('fn g17p_dynamic_kernel_va_range(');b=s.index('{',a);d=1;i=b+1
while d:d+=(s[i]=='{')-(s[i]=='}');i+=1
fn=s[a:i]
tests=r'''
use std::ops::Range;const G17P_PAGE_SIZE:usize=0x4000;
fn overlap(a:&Range<u64>,b:&Range<u64>)->bool{a.start<b.end&&b.start<a.end}
#[test]fn empty_pte_does_not_mean_free_guard_va(){let target=G17P_BOOTSTRAP_3D_STATUS_VA;let mapped=target-0x4000..target;let allocation=mapped.start..target+0x4000;let fixed=target..target+0x4000;assert!(!overlap(&mapped,&fixed));assert!(overlap(&allocation,&fixed));}
#[test]fn all_dynamic_aliases_and_guards_exclude_both_status_islands(){let full=0xfffffc2000000000..0xfffffc2100000000;let dynamic=g17p_dynamic_kernel_va_range(full.clone()).unwrap();let fixed=[G17P_BOOTSTRAP_TA_STATUS_VA..G17P_BOOTSTRAP_TA_STATUS_VA+0x4000,G17P_BOOTSTRAP_3D_STATUS_VA..G17P_BOOTSTRAP_3D_STATUS_VA+0x4000];assert_eq!(dynamic.start,0xfffffc2001684000);let mut cursor=dynamic.start;for _queue in 0..58{for bytes in[0x20000,0x400000,0x4000,0xc000,0x20000]{cursor=(cursor+0x3fff)&!0x3fff;let mapped=cursor..cursor+bytes;let allocation=cursor..mapped.end+0x4000;assert!(allocation.end<dynamic.end);for f in &fixed{assert!(!overlap(&allocation,f));}cursor=allocation.end;}}assert_eq!(dynamic.end,full.end);}
#[test]fn limited_or_pretrimmed_ranges_never_expand(){assert!(g17p_dynamic_kernel_va_range(1..2).is_none());let range=0xfffffc2040000000..0xfffffc2100000000;assert_eq!(g17p_dynamic_kernel_va_range(range.clone()),Some(range));}
'''
with tempfile.TemporaryDirectory() as d:
 f=Path(d)/'test.rs';f.write_text(constants+fn+tests);exe=Path(d)/'test';subprocess.run(['rustc','--edition=2021','--test',str(f),'-o',str(exe)],check=True);subprocess.run([str(exe)],check=True)
# Every high allocator entry point is covered. Base-address derivation and
# map_at fixed mappings remain untouched; no whole-UAT root relocation.
lines=[l.strip() for l in s.splitlines() if 'kernel_va_range()?' in l]
assert len(lines)==6,lines
assert sum('g17p_dynamic_kernel_va_range' in l for l in lines)==5,lines
assert any('let kernel_va_base = address_space.uat.kernel_va_range()?.start;'==l for l in lines)
assert 'let mapping = object.map_into_range(vm, range, alignment, prot, true)?;' in s
print('PASS all five dynamic high allocation/alias call sites exclude both fixed statuses; native base and guard behavior preserved')

manager=(p/'g17_manager.rs').read_text()
a=manager.index('    pub(crate) fn map_timestamp_buffer('); b=manager.index('\n    ///',a+1)
assert 'g17p_dynamic_kernel_va_range(' in manager[a:b]
print('PASS optional user timestamp object uses the same high-VA boundary (sixth allocation path)')
