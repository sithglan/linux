#!/usr/bin/env python3
"""Compile the actual provider functions against deterministic MMIO/OF stubs."""
# SPDX-License-Identifier: GPL-2.0-only OR MIT
from pathlib import Path
import subprocess
import tempfile
import re

root = Path(__file__).resolve().parents[4]
source = (root / 'drivers/pmdomain/apple/pmgr-pwrstate.c').read_text()

def function(name):
    start = re.search(r'(?m)^static [^{;]*\b' + re.escape(name) + r'\(', source).start()
    body = source.index('{', start)
    depth = 1
    end = body + 1
    while depth:
        depth += (source[end] == '{') - (source[end] == '}')
        end += 1
    return source[start:end]

prelude = r'''
#include <assert.h>
#include <stdbool.h>
#include <stdint.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include <errno.h>
typedef uint32_t u32;
#define __iomem
#define BIT(x) (1U << (x))
#define GENMASK(h,l) ((~0U >> (31-(h))) & (~0U << (l)))
#define FIELD_GET(mask,v) (((v)&(mask)) >> __builtin_ctz(mask))
#define FIELD_PREP(mask,v) (((v) << __builtin_ctz(mask)) & (mask))
#define container_of(p,t,m) ((t *)((char *)(p)-offsetof(t,m)))
#define genpd_to_apple_pmgr_ps(p) container_of(p,struct apple_pmgr_ps,genpd)
#define dev_info(...) ((void)0)
#define dev_err(...) ((void)0)
#define dev_dbg(...) ((void)0)
#define dev_err_probe(dev,ret,...) (ret)
struct device_node { struct device_node *parent; bool compatible; uint64_t start; };
struct device { struct device_node *of_node; };
struct generic_pm_domain { const char *name; };
struct resource { uint64_t start, end; };
struct apple_pmgr_ps {
 struct device *dev; struct generic_pm_domain genpd; void *regmap, *gfx_axi2af;
 u32 offset; bool force_disable, force_reset, externally_clocked;
};
static struct device_node parent, node, gpu_node, other_node;
static struct device dev;
static struct apple_pmgr_ps ps;
static struct device_node *found_gpu, *found_domain;
static uint64_t sgx_start, sgx_size, mapped_addr;
static int index_rc, resource_rc, maps, node_puts, map_fail;
static u32 axi[2], pmgr, values[16];
static int count, addresses[16], mmio_reads, fail_poll, fail_write, drop_bit;
static int polls, fail_busy, fail_read, stuck_bit;
static bool preserve, apple_pmgr_trace_writes;
static unsigned int apple_pmgr_suppress_write_mask, apple_pmgr_preserve_auto_probe_mask;
static bool of_device_is_compatible(struct device_node *n, const char *s) { (void)s; return n->compatible; }
static struct device_node *of_find_compatible_node(void *a, void *b, const char *s) { (void)a; (void)b; assert(!strcmp(s,"apple,agx-t8140")); return found_gpu; }
static struct device_node *of_parse_phandle(struct device_node *n, const char *s, int i) { assert(n==&gpu_node && !strcmp(s,"power-domains") && i==0); return found_domain; }
static void of_node_put(struct device_node *n) { node_puts += n!=NULL; }
static int of_property_match_string(struct device_node *n,const char *s,const char *v) { assert(n==&gpu_node && !strcmp(s,"reg-names") && !strcmp(v,"sgx")); return index_rc; }
static int of_address_to_resource(struct device_node *n,int i,struct resource *r) {
 if(n==&parent) { r->start=n->start; return 0; }
 assert(n==&gpu_node && i==index_rc); r->start=sgx_start; r->end=sgx_start+sgx_size-1; return resource_rc;
}
#define resource_size(r) ((r)->end-(r)->start+1)
static void *devm_ioremap(struct device *d,uint64_t a,unsigned int n) { assert(d==&dev && n==8); maps++; mapped_addr=a; return map_fail?NULL:axi; }
static u32 readl(void *p) { mmio_reads++; return *(u32 *)p; }
static void writel(u32 v,void *p) { int i=(u32 *)p-axi; assert(i==0||i==1); if(v&1)assert(polls>=2 && (pmgr&0xf0)==0xf0 && !(pmgr&0x800)); addresses[count]=i+1; values[count++]=v; *(u32 *)p=drop_bit?(v&~1):stuck_bit?(v|1):v; }
static int regmap_read(void *m,u32 o,u32 *v) { (void)m;(void)o; if(fail_read)return fail_read; *v=pmgr; return 0; }
static int regmap_write(void *m,u32 o,u32 v) { (void)m;(void)o; addresses[count]=0;values[count++]=v;if(!fail_write)pmgr=v;return fail_write; }
static int regmap_update_bits(void *m,u32 o,u32 mask,u32 v) { return regmap_write(m,o,(pmgr&~mask)|(v&mask)); }
static unsigned int apple_pmgr_t8140_diagnostic_bit(struct apple_pmgr_ps *p) { (void)p;return 16; }
static bool apple_pmgr_preserve_auto_on_probe(struct apple_pmgr_ps *p) { (void)p;return preserve; }
#define regmap_read_poll_timeout_atomic(map,off,cur,cond,delay,timeout) \
 ({ (void)(map);(void)(off);(void)(delay);(void)(timeout); polls++; pmgr=(pmgr&~0xf0U)|((pmgr&15)<<4); if(fail_busy)pmgr|=0x800; (cur)=pmgr; fail_poll?fail_poll:((cond)?0:-ETIMEDOUT); })
static void reset(void) {
 memset(&ps,0,sizeof(ps)); parent=(struct device_node){NULL,false,0x300700000ULL};
 node=(struct device_node){&parent,true,0}; dev.of_node=&node; ps.dev=&dev;ps.offset=0x1f0;ps.genpd.name="gfx";
 found_gpu=&gpu_node;found_domain=&node;sgx_start=0x480000000ULL;sgx_size=0x4000000;
 index_rc=1;resource_rc=maps=node_puts=map_fail=count=mmio_reads=fail_poll=fail_write=drop_bit=0;
 polls=fail_busy=fail_read=stuck_bit=0;
 pmgr=0x0f002300;axi[0]=0x10000;axi[1]=0x413b0;preserve=false;
 apple_pmgr_suppress_write_mask=0;
}
'''
defines = source[source.index('#define APPLE_PMGR_RESET '):source.index('struct apple_pmgr_ps {')]
defines += source[source.index('#define APPLE_T8140_SGX_BASE '):source.index('static int apple_pmgr_map_gfx_axi2af')]
defines += source[source.index('enum apple_pmgr_gfx_axi_action {'):source.index('static enum apple_pmgr_gfx_axi_action')]
names = ['apple_pmgr_is_t8140_gfx', 'apple_pmgr_map_gfx_axi2af', 'apple_pmgr_gfx_axi_action', 'apple_pmgr_apply_gfx_axi2af',
         'apple_pmgr_log_gfx_write', 'apple_pmgr_finish_gfx_on', 'apple_pmgr_gfx_write', 'apple_pmgr_write', 'apple_pmgr_update_bits',
         'apple_pmgr_ps_set', 'apple_pmgr_ps_power_on']
probe = source[source.index('\t/* Turn on auto-PM if the domain is already on. */'):source.index('\n\tret = pm_genpd_init')]
probe = '\nstatic int probe_active(bool active) { struct apple_pmgr_ps *p=&ps; struct device *dev=p->dev; const char *name="gfx"; int ret;' + re.sub(r'\bps\b', 'p', probe) + '\nreturn 0; }'
tests = r'''
/* Independent transcription of the native branch structure at 983c784..818. */
static int native_action(u32 old, u32 new) {
 u32 nt=new&15, ot=old&15, od=(old>>10)&1, nd=(new>>10)&1;
 if(nt!=15 || ot==15) {
  if(nd!=0 || od==0) {
   u32 changed=od!=nd?nd:0;
   if((ot==nt || nt!=0) && changed==0)return APPLE_PMGR_GFX_AXI_NONE;
   return APPLE_PMGR_GFX_AXI_CLEAR_BEFORE;
  }
 }
 return APPLE_PMGR_GFX_AXI_SET_AFTER;
}
int main(void) {
 for(u32 old=0;old<16;old++)for(u32 new=0;new<16;new++)for(u32 od=0;od<2;od++)for(u32 nd=0;nd<2;nd++) {
  u32 a=old|(od<<10), b=new|(nd<<10);
  assert(apple_pmgr_gfx_axi_action(a,b)==native_action(a,b));
  assert(apple_pmgr_gfx_axi_action(a|0x1f002bf0,b|0x0f0023f0)==native_action(a,b));
 }
 reset();assert(apple_pmgr_map_gfx_axi2af(&ps)==0);assert(maps==1 && mapped_addr==0x481000104ULL && !mmio_reads);
 assert(apple_pmgr_ps_power_on(&ps.genpd)==0);
 assert(count==4 && addresses[0]==0 && addresses[1]==1 && addresses[2]==2 && addresses[3]==0);
 assert(values[0]==0x0f00200f && values[1]==0x10001 && values[2]==0x413b1 && values[3]==0x1f00200f);
 assert(apple_pmgr_ps_set(&ps.genpd,0,false)==0 && count==7 && mmio_reads==8);
 assert(addresses[4]==1 && addresses[5]==2 && addresses[6]==0 && values[4]==0x10000 && values[5]==0x413b0 && (values[6]&15)==0);
 assert(apple_pmgr_ps_power_on(&ps.genpd)==0 && count==11 && mmio_reads==12);
 reset();assert(!apple_pmgr_map_gfx_axi2af(&ps));pmgr=0x1f0023ff;assert(!probe_active(true));assert(count==4 && addresses[1]==1 && addresses[2]==2 && (values[0]&(1U<<28))==0 && (values[3]&(1U<<28)));
 reset();assert(!apple_pmgr_map_gfx_axi2af(&ps));pmgr=0x0f0023f0;assert(!probe_active(true));assert(count==4 && mmio_reads==4); /* no duplicate restore after target transition */
 reset();assert(!apple_pmgr_map_gfx_axi2af(&ps));assert(!probe_active(false) && count==0 && !mmio_reads);
 reset();assert(!apple_pmgr_map_gfx_axi2af(&ps));fail_poll=-ETIMEDOUT;assert(apple_pmgr_ps_power_on(&ps.genpd)==-ETIMEDOUT && count==1 && !mmio_reads);
 reset();assert(!apple_pmgr_map_gfx_axi2af(&ps));fail_write=-EIO;assert(apple_pmgr_ps_power_on(&ps.genpd)==-EIO && count==1 && !mmio_reads);
 reset();assert(!apple_pmgr_map_gfx_axi2af(&ps));fail_busy=1;assert(apple_pmgr_ps_power_on(&ps.genpd)==-ETIMEDOUT && count==1 && !mmio_reads && polls==2);
 reset();assert(!apple_pmgr_map_gfx_axi2af(&ps));fail_read=-EIO;assert(apple_pmgr_ps_power_on(&ps.genpd)==-EIO && count==0 && !mmio_reads);
 reset();assert(!apple_pmgr_map_gfx_axi2af(&ps));drop_bit=1;assert(apple_pmgr_ps_power_on(&ps.genpd)==-EIO && count==2 && mmio_reads==2);
 reset();assert(!apple_pmgr_map_gfx_axi2af(&ps));pmgr=0x1f0023ff;axi[0]|=1;axi[1]|=1;
 assert(!apple_pmgr_update_bits(&ps,APPLE_PMGR_DEV_DISABLE,APPLE_PMGR_DEV_DISABLE,"disable"));
 assert(count==3 && addresses[0]==1 && addresses[1]==2 && addresses[2]==0 && (pmgr&0x400));
 assert(!apple_pmgr_update_bits(&ps,APPLE_PMGR_DEV_DISABLE,0,"enable"));
 assert(count==6 && addresses[3]==0 && addresses[4]==1 && addresses[5]==2 && !(pmgr&0x400));
 reset();assert(!apple_pmgr_map_gfx_axi2af(&ps));pmgr=0x1f0023ff;axi[0]|=1;axi[1]|=1;stuck_bit=1;
 assert(apple_pmgr_ps_set(&ps.genpd,0,false)==-EIO && count==1 && addresses[0]==1 && (pmgr&15)==15);
 reset();assert(!apple_pmgr_map_gfx_axi2af(&ps));pmgr=0x1f0023ff;
 assert(!apple_pmgr_update_bits(&ps,APPLE_PMGR_AUTO_ENABLE,APPLE_PMGR_AUTO_ENABLE,"unchanged") && !mmio_reads);
 /* Not used by live GFX: generic force-disable's final OFF clears DEV_DISABLE,
  * so the exact native predicate requires ACTIVE. Fail, do not invent policy. */
 reset();assert(!apple_pmgr_map_gfx_axi2af(&ps));pmgr=0x1f0023ff;axi[0]|=1;axi[1]|=1;ps.force_disable=true;
 assert(apple_pmgr_ps_set(&ps.genpd,0,false)==-ETIMEDOUT && count==4 && mmio_reads==4 && !(axi[0]&1) && !(axi[1]&1));
 reset();apple_pmgr_suppress_write_mask=16;assert(!apple_pmgr_write(&ps,15,"suppressed") && !mmio_reads && !count);
 for(int which=0;which<7;which++) {
  reset();if(which==0)found_gpu=NULL;if(which==1)found_domain=&other_node;if(which==2)index_rc=-EINVAL;
  if(which==3)resource_rc=-EINVAL;if(which==4)sgx_start++;if(which==5)sgx_size=0x100010b;if(which==6)map_fail=1;
  assert(apple_pmgr_map_gfx_axi2af(&ps)<0 && !ps.gfx_axi2af && !mmio_reads && !count);
 }
 for(int which=0;which<3;which++) {
  reset();if(which==0)ps.offset=0x240;if(which==1)node.compatible=false;if(which==2)parent.start++;
  assert(!apple_pmgr_map_gfx_axi2af(&ps) && !maps && !ps.gfx_axi2af);
  assert(!apple_pmgr_ps_power_on(&ps.genpd) && count==2 && !mmio_reads);
  count=0;assert(!probe_active(true) && count==1 && !mmio_reads);
 }
 reset();assert(apple_pmgr_apply_gfx_axi2af(&ps,true)==-ENODEV && !mmio_reads);
 assert(apple_pmgr_write(&ps,15,"unmapped")==-ENODEV && !mmio_reads && !count);
 printf("1024 native predicate combinations plus flags, full/masked ordering, failure, mapping and generic-domain checks passed\n");return 0;
}
'''
assert source.index('ret = apple_pmgr_map_gfx_axi2af(ps);') < source.index('active = apple_pmgr_ps_is_active(ps);')
regs = (root / 'drivers/gpu/drm/asahi/regs.rs').read_text()
validation = regs[regs.index('pub(crate) fn validate_t8140_axi_transition'):regs.index('pub(crate) fn t8140_pre_initdata_clear')]
assert 'try_write' not in validation and 'return Err(EIO)' in validation
code = prelude + defines + ''.join(function(n) for n in names) + probe + tests
with tempfile.TemporaryDirectory(prefix='pmgr-axi-test-') as directory:
    binary = Path(directory) / 'test'
    subprocess.run(['clang','-std=gnu11','-Wall','-Wno-unused-variable','-Wno-unused-parameter','-x','c','-','-o',str(binary)],input=code,text=True,check=True)
    subprocess.run([str(binary)],check=True)
