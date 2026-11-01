// SPDX-License-Identifier: GPL-2.0-only
/*
 * neo_clpc: T8140 (J700) CPU/GPU memory- and fabric-demand floors on the PMC
 * CLPC voter agent, modelled on macOS AppleT8140CLPC.
 *
 * Native evidence (27.2 kernelcache, com.apple.driver.AppleT8140CLPC):
 *  - clpc::memory::DVFManager (DCS) and clpc::soc::DVFManager (fabric) take
 *    requests from the CPU clusters (raise*FromACC / *FromPerfLevel), the ANE
 *    and "System" (ADT clpc system-utilization-gpu-in-en = 1: GPU work), and
 *    requestPerformanceChange() ends in
 *    clpc::pmgr::PMCVoterInterface::setSocPerfStateFloor (0x990f00c) /
 *    setMemoryPerfStateFloor (0x990f22c) -> writePerfStateFloorReg
 *    (0x990f09c): floor word = DCS[3:0] | SOC[7:4] | ANE<<16, the max over
 *    the ACC, System and ANE sources, written to the register returned by
 *    pmcCLPCPerfStateFloorRegister() (init string).
 *  - [inference] that register is the PMC perf-state control floor of voter
 *    agent CLPC (ADT pmgr clpc-voter-agent-idx = 1) = PMC + (1 << 14) =
 *    0x302804000, whose nibble layout (DCS 0, SOC 1) matches
 *    ApplePMC::setPerfStateAgent.  neo_pmc active=1 enables bit 1 (CLPC) on
 *    every rail, as restorePMC does.
 *  - The native mapping from cluster perf level / GPU work to DCS/SOC levels
 *    (DVFMTable, mem/fab-compute-power-target, mem-cpu-stall PI) is NOT
 *    decoded: the tables below are provisional, table-driven and replaced
 *    from the macOS capture (pmgr_votes.d probes the two floor setters).
 *
 * Linux sources (tied to real activity, released at idle -- not a hold):
 *  - CPU: every sample_ms (ADT clpc sampling-interval-ms = 4), per cpufreq
 *    policy: busy = max over its CPUs of (1 - idle/wall); level =
 *    cur/max frequency.  A cluster demands only while busy >= busy_pct.
 *  - GPU ("System"): opt-in (gpu_soc/gpu_dcs, default 0 = off): while the
 *    GFX power state is active and the GPU performance state (sgx+0xe01000)
 *    is non-zero.
 *  - Release after hold_ms without demand.  Unload writes the saved floor.
 *  - Diagnostic fixed override (opt-in, NOT a default): force_dcs/force_soc
 *    (-1 = off) or debugfs "force" ("DCS SOC", "-1" to clear) hold the floor
 *    at fixed levels regardless of demand, e.g. for throughput-ceiling
 *    sweeps.  Forced writes are read back; unload restores the saved floor.
 */
#include <linux/cpufreq.h>
#include <linux/debugfs.h>
#include <linux/io.h>
#include <linux/kernel_stat.h>
#include <linux/module.h>
#include <linux/of.h>
#include <linux/seq_file.h>
#include <linux/tick.h>
#include <linux/uaccess.h>
#include <linux/workqueue.h>

#define PMC_CLPC_FLOOR	0x302804000ULL	/* PMC + (agent 1 << 14) */
#define PMC_ENABLE	0x3028ac000ULL	/* + 4 * rail, bit 1 = CLPC */
#define PMGR_PS_GFX	0x3007001f0ULL
#define SGX_GPC_STATE	0x480e01000ULL
#define RAIL_DCS	0
#define RAIL_SOC	1

static bool active;
module_param(active, bool, 0444);
MODULE_PARM_DESC(active, "Write the CLPC-agent floor (default 0: compute and report only)");
static unsigned int sample_ms = 4;
module_param(sample_ms, uint, 0444);
MODULE_PARM_DESC(sample_ms, "Sample period, ms (ADT clpc sampling-interval-ms = 4)");
static unsigned int busy_pct = 50;
module_param(busy_pct, uint, 0444);
MODULE_PARM_DESC(busy_pct, "Cluster busy threshold, percent of the sample");
static unsigned int hold_ms = 32;
module_param(hold_ms, uint, 0444);
MODULE_PARM_DESC(hold_ms, "Keep the last floor this long after demand stops, ms");
/* Provisional (not native): floor by cluster frequency quartile. */
static char *dcs_map = "1,2,3,4";
module_param(dcs_map, charp, 0444);
MODULE_PARM_DESC(dcs_map, "DCS floor for busy clusters at <25,<50,<75,>=75 %% of max frequency (provisional)");
static char *soc_map = "0,0,0,0";
module_param(soc_map, charp, 0444);
MODULE_PARM_DESC(soc_map, "SOC floor for busy clusters by frequency quartile (provisional; native unknown)");
static unsigned int gpu_dcs;
module_param(gpu_dcs, uint, 0444);
MODULE_PARM_DESC(gpu_dcs, "DCS floor while the GPU runs (opt-in, 0 = off; native System source unknown)");
static unsigned int gpu_soc;
module_param(gpu_soc, uint, 0444);
MODULE_PARM_DESC(gpu_soc, "SOC floor while the GPU runs (opt-in, 0 = off; native System source unknown)");
static int force_dcs = -1;
module_param(force_dcs, int, 0444);
MODULE_PARM_DESC(force_dcs, "DIAGNOSTIC: hold the DCS floor at this level 0..4 (-1 = demand-driven)");
static int force_soc = -1;
module_param(force_soc, int, 0444);
MODULE_PARM_DESC(force_soc, "DIAGNOSTIC: hold the SOC floor at this level 0..3 (-1 = demand-driven)");

#define MAX_CL 4
struct cluster {
	unsigned int cpu;	/* policy->cpu */
	unsigned int ncpu;
	unsigned int cpus[16];
	u64 idle[16], wall[16];
	unsigned int busy_pct, level_pct, dcs, soc;
};

static void __iomem *floor_reg, *en_reg, *ps_gfx, *gpc;
static struct cluster cl[MAX_CL];
static unsigned int ncl;
static u8 dmap[4], smap[4];
static u32 saved_floor, cur_floor;
static unsigned int want_dcs, want_soc, gpu_on;
static unsigned long last_demand, n_writes, n_samples, n_readback_bad;
static int fdcs = -1, fsoc = -1;	/* live override, set from params or debugfs */
static struct delayed_work work;
static struct dentry *dir;

static int parse_map(const char *s, u8 *out, unsigned int max)
{
	unsigned int v[4];

	if (sscanf(s, "%u,%u,%u,%u", &v[0], &v[1], &v[2], &v[3]) != 4)
		return -EINVAL;
	for (int i = 0; i < 4; i++) {
		if (v[i] > max)
			return -EINVAL;
		out[i] = v[i];
	}
	return 0;
}

static void sample_cluster(struct cluster *c)
{
	struct cpufreq_policy *p = cpufreq_cpu_get(c->cpu);
	unsigned int i, busy = 0;

	for (i = 0; i < c->ncpu; i++) {
		u64 wall, idle = get_cpu_idle_time(c->cpus[i], &wall, 0);
		u64 dw = wall - c->wall[i], di = idle - c->idle[i];

		c->wall[i] = wall;
		c->idle[i] = idle;
		if (dw && di <= dw)
			busy = max_t(unsigned int, busy, (unsigned int)div64_u64((dw - di) * 100, dw));
	}
	c->busy_pct = busy;
	c->level_pct = 0;
	if (p) {
		if (p->cpuinfo.max_freq)
			c->level_pct = p->cur * 100 / p->cpuinfo.max_freq;
		cpufreq_cpu_put(p);
	}
	if (busy >= busy_pct) {
		unsigned int q = min_t(unsigned int, c->level_pct / 25, 3);

		c->dcs = dmap[q];
		c->soc = smap[q];
	} else {
		c->dcs = c->soc = 0;
	}
}

static bool gpu_running(void)
{
	u32 ps = readl(ps_gfx);

	/* PS actual nibble must be active before touching sgx (SError-safe). */
	if (((ps >> 4) & 0xf) != 0xf)
		return false;
	return (readl(gpc) & 0xf) != 0;
}

static void write_floor(unsigned int dcs, unsigned int soc)
{
	u32 v = (cur_floor & ~0xffu) | (dcs & 0xf) | ((soc & 0xf) << 4);

	if (v == cur_floor)
		return;
	writel(v, floor_reg);
	cur_floor = readl(floor_reg);
	n_writes++;
	if ((cur_floor & 0xff) != (v & 0xff)) {
		n_readback_bad++;
		pr_warn_ratelimited("neo_clpc: floor readback %#x != %#x\n", cur_floor, v);
	}
}

static void sample_fn(struct work_struct *w)
{
	unsigned int i, dcs = 0, soc = 0;

	n_samples++;
	for (i = 0; i < ncl; i++) {
		sample_cluster(&cl[i]);
		dcs = max(dcs, cl[i].dcs);
		soc = max(soc, cl[i].soc);
	}
	gpu_on = (gpu_dcs || gpu_soc) ? gpu_running() : 0;
	if (gpu_on) {
		dcs = max(dcs, gpu_dcs);
		soc = max(soc, gpu_soc);
	}
	if (dcs || soc) {
		last_demand = jiffies;
		want_dcs = dcs;
		want_soc = soc;
	} else if (time_after(jiffies, last_demand + msecs_to_jiffies(hold_ms))) {
		want_dcs = want_soc = 0;
	} else {
		/* Hold: keep the previous request until hold_ms passes. */
	}
	if (READ_ONCE(fdcs) >= 0 || READ_ONCE(fsoc) >= 0) {
		/* Diagnostic hold: forced rails fixed, the other stays demand-driven. */
		int d = READ_ONCE(fdcs), so = READ_ONCE(fsoc);

		if (d >= 0)
			want_dcs = d;
		if (so >= 0)
			want_soc = so;
	}
	if (active)
		write_floor(want_dcs, want_soc);
	queue_delayed_work(system_highpri_wq, &work, msecs_to_jiffies(sample_ms));
}

static int status_show(struct seq_file *s, void *unused)
{
	unsigned int i;

	seq_printf(s, "active=%u floor=%#x saved=%#x want dcs=%u soc=%u gpu_on=%u writes=%lu samples=%lu readback_bad=%lu\n",
		   active, readl(floor_reg), saved_floor, want_dcs, want_soc, gpu_on, n_writes, n_samples,
		   n_readback_bad);
	seq_printf(s, "force dcs=%d soc=%d (DIAGNOSTIC hold; -1 = demand-driven)\n", READ_ONCE(fdcs),
		   READ_ONCE(fsoc));
	seq_printf(s, "enable DCS=%#x SOC=%#x (bit1 = CLPC voter)\n", readl(en_reg + 4 * RAIL_DCS),
		   readl(en_reg + 4 * RAIL_SOC));
	seq_printf(s, "dcs_map=%u,%u,%u,%u soc_map=%u,%u,%u,%u gpu_dcs=%u gpu_soc=%u (provisional)\n",
		   dmap[0], dmap[1], dmap[2], dmap[3], smap[0], smap[1], smap[2], smap[3], gpu_dcs, gpu_soc);
	for (i = 0; i < ncl; i++)
		seq_printf(s, "cluster cpu%u ncpu=%u busy=%u%% level=%u%% -> dcs=%u soc=%u\n", cl[i].cpu,
			   cl[i].ncpu, cl[i].busy_pct, cl[i].level_pct, cl[i].dcs, cl[i].soc);
	return 0;
}
DEFINE_SHOW_ATTRIBUTE(status);

/* debugfs "force": "DCS SOC" (each -1..max) or "-1" to clear both. */
static ssize_t force_write(struct file *f, const char __user *ubuf, size_t len, loff_t *pos)
{
	char buf[32];
	int d, so, n;

	if (len >= sizeof(buf) || copy_from_user(buf, ubuf, len))
		return -EINVAL;
	buf[len] = 0;
	n = sscanf(buf, "%d %d", &d, &so);
	if (n == 1 && d == -1)
		so = -1;
	else if (n != 2)
		return -EINVAL;
	if (d < -1 || d > 4 || so < -1 || so > 3)
		return -EINVAL;
	WRITE_ONCE(fdcs, d);
	WRITE_ONCE(fsoc, so);
	pr_info("neo_clpc: DIAGNOSTIC force dcs=%d soc=%d\n", d, so);
	return len;
}

static const struct file_operations force_fops = { .owner = THIS_MODULE, .write = force_write };

static void unmap_all(void)
{
	if (gpc)
		iounmap(gpc);
	if (ps_gfx)
		iounmap(ps_gfx);
	if (en_reg)
		iounmap(en_reg);
	if (floor_reg)
		iounmap(floor_reg);
}

static int __init clpc_init(void)
{
	unsigned int cpu, i;
	int ret;

	if (!of_machine_is_compatible("apple,j700"))
		return -ENODEV;
	ret = parse_map(dcs_map, dmap, 4) ?: parse_map(soc_map, smap, 3);
	if (ret || gpu_dcs > 4 || gpu_soc > 3 || force_dcs < -1 || force_dcs > 4 ||
	    force_soc < -1 || force_soc > 3)
		return -EINVAL;
	fdcs = force_dcs;
	fsoc = force_soc;
	floor_reg = ioremap(PMC_CLPC_FLOOR, 8);
	en_reg = ioremap(PMC_ENABLE, 0x10);
	ps_gfx = ioremap(PMGR_PS_GFX, 4);
	gpc = ioremap(SGX_GPC_STATE, 4);
	if (!floor_reg || !en_reg || !ps_gfx || !gpc) {
		unmap_all();
		return -ENOMEM;
	}
	for_each_possible_cpu(cpu) {
		struct cpufreq_policy *p = cpufreq_cpu_get(cpu);

		if (!p)
			continue;
		for (i = 0; i < ncl && cl[i].cpu != p->cpu; i++)
			;
		if (i == ncl && ncl < MAX_CL)
			cl[ncl++].cpu = p->cpu;
		if (i < MAX_CL && cl[i].ncpu < ARRAY_SIZE(cl[i].cpus))
			cl[i].cpus[cl[i].ncpu++] = cpu;
		cpufreq_cpu_put(p);
	}
	for (i = 0; i < ncl; i++)
		for (cpu = 0; cpu < cl[i].ncpu; cpu++)
			cl[i].idle[cpu] = get_cpu_idle_time(cl[i].cpus[cpu], &cl[i].wall[cpu], 0);
	saved_floor = cur_floor = readl(floor_reg);
	if (active && (!(readl(en_reg + 4 * RAIL_DCS) & BIT(1)) || !(readl(en_reg + 4 * RAIL_SOC) & BIT(1))))
		pr_warn("neo_clpc: CLPC voter not enabled on DCS/SOC (load neo_pmc active=1): floor has no effect\n");
	dir = debugfs_create_dir("neo_clpc", NULL);
	debugfs_create_file("status", 0400, dir, NULL, &status_fops);
	if (active)
		debugfs_create_file("force", 0200, dir, NULL, &force_fops);
	INIT_DELAYED_WORK(&work, sample_fn);
	queue_delayed_work(system_highpri_wq, &work, msecs_to_jiffies(sample_ms));
	pr_info("neo_clpc: %s, %u clusters, saved floor %#x\n", active ? "active" : "observe-only", ncl,
		saved_floor);
	return 0;
}

static void __exit clpc_exit(void)
{
	cancel_delayed_work_sync(&work);
	debugfs_remove_recursive(dir);
	if (active) {
		writel(saved_floor, floor_reg);
		readl(floor_reg);
	}
	unmap_all();
}

module_init(clpc_init);
module_exit(clpc_exit);
MODULE_LICENSE("GPL");
MODULE_DESCRIPTION("T8140 CPU/GPU demand floors on the PMC CLPC voter, modelled on AppleT8140CLPC");
