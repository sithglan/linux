/* SPDX-License-Identifier: GPL-2.0-only OR MIT */
/*
 * T8140 (J700, Neo) memory-system / PMGR / PMC / PMP register map used by the
 * read-only fabric observers.  Shared by the userspace /dev/mem sampler, the
 * host unit test and the kernel observer module.
 *
 * Every address below is derived from inspected sources; see FABRIC.md
 * (work/neo-gpu-throughput-20260929/fabric-speed-20260929/) for the chain of
 * evidence.  Summary:
 *
 *   PMGR PS registers: J700 ADT /arm-io/pmgr reg[0] = 0x300700000 and the
 *   ADT "devices" table (PMC ps 0x138 id 0x11, GFX ps 0x1f0 id 0x28, PMP ps
 *   0x3d0 id 0x64, PMS_SRAM ps 0x3d8); identical offsets in the Linux
 *   t8140-pmgr.dtsi (ps_pmc@138, ps_gfx@1f0, ps_pmp@3d0).  Field layout
 *   ACTUAL[7:4] TARGET[3:0], 0xf = active (drivers/gpu/drm/apple/dcp.c:1375).
 *
 *   PMP ASC CPU_CONTROL: ADT /arm-io/pmp reg[0] = 0x300e00000, +0x44 bit 4
 *   RUN (drivers/soc/apple/pmp-v2.c PMP_CPU_CONTROL / PMP_CPU_RUN).
 *
 *   PMC base 0x302800000: AppleT8140PMGR::initRegMaps maps RegMap 0x81 to
 *   ADT pmgr reg[43] (0x302000000, 16 MiB); AppleT8140PMGR::initRegGroups
 *   sets RegGroup 0x22 = {RegMap 0x81, +0x800000}; ApplePMC::readReg32 /
 *   writeReg32 use that group ([pmgr+0x6080..0x6088]).  Corroborated by
 *   ADT disp0 reg[5] / dispext0 reg[4] = 0x302800000 size 0xbc000.
 *   Offsets from AppleT8140PMC: perf-state control floor per voter agent
 *   (agent << 14), pending +4; rail state 0xaa000 + 4*rail
 *   (readPMCStateReg); interface enables 0xac000 + 4*rail (type 0,
 *   read/writeEnableRegister), 0xac020/0xac024/0xac028 (types 1..3).
 *   Voter agents (ADT pmgr voter-agent): PMP 0, CLPC 1, GFX 2 (dynamic,
 *   device 0x28, rails 0xf), AP 3.  Rails (voter-rail-data): DCS 0, SOC 1,
 *   DISP 2, AVE 3; 4-bit nibble per rail in a vote word.
 *
 *   GPC performance state: sgx + 0xe01000 & 0xf (drivers/gpu/drm/asahi
 *   regs.rs, sampled in v55 at idle and under load).
 *
 *   MCC cache status: ADT /chosen/lock-regs/amcc aperture 0x220000000,
 *   plane-count 4, plane-stride 0x40000, cache-status-reg-offset 0x2800,
 *   mask 0x1f (iBoot lock-register description; semantic of the field is
 *   NOT decoded -- raw value only).
 */
#ifndef NEO_FABRIC_REGS_H
#define NEO_FABRIC_REGS_H

#ifdef __KERNEL__
#include <linux/types.h>
typedef u32 nf_u32;
typedef u64 nf_u64;
#else
#include <stdint.h>
typedef uint32_t nf_u32;
typedef uint64_t nf_u64;
#endif

#define NF_PMGR_BASE          0x300700000ULL
#define NF_PMGR_PS_PMS        0x130
#define NF_PMGR_PS_PMC        0x138
#define NF_PMGR_PS_GFX        0x1f0
#define NF_PMGR_PS_PMP        0x3d0
#define NF_PMGR_PS_PMS_SRAM   0x3d8

#define NF_PMP_ASC_BASE       0x300e00000ULL
#define NF_PMP_CPU_CONTROL    0x44
#define NF_PMP_CPU_RUN        (1u << 4)

#define NF_PMC_BASE           0x302800000ULL
#define NF_PMC_SIZE           0xbc000ULL
#define NF_PMC_AGENT_STRIDE   0x4000u     /* agent << 14 */
#define NF_PMC_AGENT_PENDING  0x4u
#define NF_PMC_RAIL_STATE     0xaa000u    /* + 4 * rail */
#define NF_PMC_IF_ENABLE      0xac000u    /* + 4 * rail, interface type 0 */
#define NF_PMC_IF_ENABLE_T1   0xac020u
#define NF_PMC_IF_ENABLE_T2   0xac024u
#define NF_PMC_IF_ENABLE_T3   0xac028u
#define NF_PMC_ATC_DCS_FLOOR  0x78000u    /* AppleT8140PMC::stateChange devs 0x153..0x157 */

enum nf_agent { NF_AGENT_PMP = 0, NF_AGENT_CLPC = 1, NF_AGENT_GFX = 2, NF_AGENT_AP = 3, NF_AGENTS = 4 };
enum nf_rail { NF_RAIL_DCS = 0, NF_RAIL_SOC = 1, NF_RAIL_DISP = 2, NF_RAIL_AVE = 3, NF_RAILS = 4 };

#define NF_GFX_AGENT_BIT      2u   /* voter-agent GFX byte2 */
#define NF_GFX_AGENT_RAILMASK 0xfu /* voter-agent GFX byte1 */

#define NF_SGX_BASE           0x480000000ULL
#define NF_SGX_GPC_STATE      0xe01000u

/*
 * AMCC (memory-cache controller, SLC).  ADT mcc reg[amcc-reg-idx=2] =
 * 0x220000000 size 0x140000; 4 planes x 0x40000; broadcast window at
 * +0x100000 (MemCacheCIP::protectedWriteBroadcastPlaneReg32 adds 0x100000).
 * macOS AppleT8140MCC MemCacheCIP::mccEnableCacheMode(true): broadcast
 * write of the way count to +0x2800, poll every plane's +0x2804 until it
 * equals the way count (mccWaitForWaysPwrOn), then broadcast write 0 to
 * +0x82c.  SPTM assert_amcc_cache_disabled expects (+0x2800 & 0x1f) == 0 at
 * lockdown, i.e. iBoot hands off with the SLC disabled.  Way count default
 * 12 (ADT max-way-count absent on J700).  DSID group quota g at plane +
 * 0xa000 + 4*g, g < 32 (MemCacheCIP::getDSIDGroupQuota).
 */
#define NF_MCC_BASE           0x220000000ULL
#define NF_MCC_SIZE           0x140000u
#define NF_MCC_PLANES         4
#define NF_MCC_PLANE_STRIDE   0x40000u
#define NF_MCC_BROADCAST      0x100000u
#define NF_MCC_CACHE_STATUS   0x2800u     /* way-enable count (lock-regs cache-status) */
#define NF_MCC_CACHE_MASK     0x1fu
#define NF_MCC_WAYS_POWERED   0x2804u
#define NF_MCC_POST_ENABLE    0x82cu
#define NF_MCC_DSID_QUOTA     0xa000u
#define NF_MCC_DSID_GROUPS    32
#define NF_MCC_DEFAULT_WAYS   12u
/*
 * Read-only SLC allocation observers (AppleT8140MCC MemCacheCIP, 27.2):
 *  +0x2804 bits[30:24] = ways powered (mccWaitForWaysPwrOn ubfx #24,#7;
 *          a second field sits in bits[14:8]); NOT a plain way count.
 *  +0xa400 + 4*g (g < 32): DSID-group occupancy, (v & 0x3ffff) << 7 bytes
 *          (getDSIDGroupQuotaUseCount 0xfffffe0009923c4c).
 *  +0xc000 + 4*d (d < 192): per-DSID occupancy, same units
 *          (getDSIDQuotaUseCount 0xfffffe0009923f54).
 *  +0xa000 + 4*g: group quota; setDSIDGroupQuota writes bytes >> 17 into a
 *          6-bit field (0xfffffe0009924690).
 *  +0x14000 + 4*d: per-DSID config word (deprioritizeDSID 0xfffffe0009924820).
 */
#define NF_MCC_GROUP_USE      0xa400u
#define NF_MCC_DSID_USE       0xc000u
#define NF_MCC_DSID_CFG       0x14000u
#define NF_MCC_DSIDS          192

static inline nf_u64 nf_mcc_plane(unsigned int plane, nf_u32 off)
{
	return NF_MCC_BASE + (nf_u64)plane * NF_MCC_PLANE_STRIDE + off;
}

static inline unsigned int nf_ps_target(nf_u32 v) { return v & 0xf; }
static inline unsigned int nf_ps_actual(nf_u32 v) { return (v >> 4) & 0xf; }
static inline int nf_ps_active(nf_u32 v) { return nf_ps_actual(v) == 0xf; }
/* A value of all-ones means the read did not reach a live register. */
static inline int nf_ps_plausible(nf_u32 v) { return v != 0xffffffffu; }

/* 4-bit nibble for @rail in a PMC vote/floor word (ApplePMC::setPerfStateAgent). */
static inline unsigned int nf_vote_nibble(nf_u32 vote, unsigned int rail)
{
	return (vote >> (4 * rail)) & 0xf;
}

static inline nf_u32 nf_vote_set_nibble(nf_u32 vote, unsigned int rail, unsigned int state)
{
	nf_u32 shift = 4 * rail;

	return (vote & ~(0xfu << shift)) | ((state & 0xfu) << shift);
}

/* Interface enable word with @agent's bit set/cleared (ApplePMC::_enableDynamicVoterInterface). */
static inline nf_u32 nf_enable_with_agent(nf_u32 enable, unsigned int agent, int on)
{
	return on ? (enable | (1u << agent)) : (enable & ~(1u << agent));
}

static inline nf_u64 nf_pmc_agent_floor(unsigned int agent)
{
	return NF_PMC_BASE + (nf_u64)agent * NF_PMC_AGENT_STRIDE;
}

static inline nf_u64 nf_pmc_rail_state(unsigned int rail)
{
	return NF_PMC_BASE + NF_PMC_RAIL_STATE + 4ull * rail;
}

static inline nf_u64 nf_pmc_enable(unsigned int rail)
{
	return NF_PMC_BASE + NF_PMC_IF_ENABLE + 4ull * rail;
}

/*
 * ADT pmgr "voter-agent" record (24 bytes): type (1 static, 2 dynamic),
 * rail mask, agent bit, id, u32 device id, 16-byte name.
 */
struct nf_voter_agent {
	unsigned int type, rail_mask, bit, id, device;
	char name[17];
};

static inline int nf_parse_voter_agent(const unsigned char *rec, struct nf_voter_agent *out)
{
	unsigned int i;

	out->type = rec[0];
	out->rail_mask = rec[1];
	out->bit = rec[2];
	out->id = rec[3];
	out->device = rec[4] | rec[5] << 8 | rec[6] << 16 | (nf_u32)rec[7] << 24;
	for (i = 0; i < 16; i++)
		out->name[i] = (char)rec[8 + i];
	out->name[16] = 0;
	return (out->type == 1 || out->type == 2) && out->bit < 32 ? 0 : -1;
}

#endif
