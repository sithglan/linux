// SPDX-License-Identifier: GPL-2.0-only
/*
 * neo_pmc: T8140 (J700) PMC voter-interface enables, modelled on macOS.
 *
 * macOS (27.2 com.apple.driver.ApplePMGR / AppleT8140PMGR):
 *  - ApplePMGR::restoreHW -> PMC vtable+0x28 AppleT8140PMC::restorePMC ->
 *    ApplePMC::restorePMC (0xfffffe0009535478): with pmc-pmgr bit 0 set it
 *    calls _enableStaticVoterInterface(idx, true) for the ADT
 *    pmp-/clpc-/ap-voter-agent-idx agents.  That sets (1 << agent.bit) in the
 *    PMC interface-type-0 enable word of every rail in the agent's rail mask
 *    (PMC + 0xac000 + 4*rail; read/writeEnableRegister 0x994d5c0/0x994d52c).
 *  - ApplePMGR::_enableDeviceGated -> _notifyPMC -> _notifyPMCGated ->
 *    ApplePMC::stateChange(on, dev) -> _enableDynamicVoterInterface(on, dev):
 *    the dynamic GFX voter (device 0x28, bit 2) is SET when the host powers
 *    the GFX PMGR device on and CLEARED when it powers it off
 *    (csel set/clear at 0x9535964..74).
 *
 * Native ordering (_enableDeviceGated 0xfffffe00095108c8): the PS write is
 *  _syncDeviceStatusChange -> _setPSLevel (call at +0x4cc).  _notifyPMC is
 *  called in a loop before it (+0x34c) and after it (+0x6bc); ADT pmgr
 *  device flag bit 6 selects which.  GFX (id 0x28, ps 0x1f0) has flags 0x82
 *  (bit 6 clear), so natively: set AFTER the power-on PS write, clear BEFORE
 *  the power-off PS write, synchronously (runAction on the PMC gate).
 *  _notifyPMC has no other callers; the gate at +0x174 compares the
 *  software-requested state, so hardware auto-gating below the host target
 *  is not a PMC event natively either.
 *
 * Linux equivalents:
 *  - static voters: set once at load (restorePMC happens at PMGR init/wake).
 *  - GFX voter: GENPD_NOTIFY_ON (sent after ->power_on succeeded) sets,
 *    GENPD_NOTIFY_PRE_OFF (sent before ->power_off) clears -- the native
 *    order.  The gfx genpd is GENPD_FLAG_IRQ_SAFE, so these notifiers run
 *    under genpd's spinlock with IRQs off: the handler only takes a raw
 *    spinlock and does bounded MMIO (<= 4 RMW + readback).  Deferring to a
 *    work item would break both orderings (GPU use before set; clear after
 *    the rail is gone), so it is not used.
 *
 * Voter-agent and rail tables are read from the ADT-derived constants in
 * fabric_regs.h (J700 live ADT pmgr voter-agent / voter-rail-data).
 * Every write is read back.  Unload restores the enable words saved at load.
 * active=0 (module parameter) gives a read-only status file only.
 */
#include <linux/debugfs.h>
#include <linux/io.h>
#include <linux/module.h>
#include <linux/spinlock.h>
#include <linux/notifier.h>
#include <linux/of.h>
#include <linux/of_platform.h>
#include <linux/platform_device.h>
#include <linux/pm_domain.h>
#include <linux/seq_file.h>

#include "fabric_regs.h"

int neo_pmc_gfx_set(bool on);

static bool active;
module_param(active, bool, 0444);
MODULE_PARM_DESC(active, "Apply the macOS restorePMC static-voter enables and track GFX power (default 0: status only)");

static bool gfx_voter = true;
module_param(gfx_voter, bool, 0444);
MODULE_PARM_DESC(gfx_voter, "With active=1, drive the GFX dynamic voter from ps_gfx genpd events");

/* J700 ADT voter-agent: PMP bit0, CLPC bit1, GFX bit2 (dynamic, dev 0x28), AP bit3; all rail mask 0xf. */
static const unsigned int static_agents[] = { NF_AGENT_PMP, NF_AGENT_CLPC, NF_AGENT_AP };
#define RAIL_MASK 0xfu

static void __iomem *pmgr, *pmc;
static struct dentry *dir;
static DEFINE_RAW_SPINLOCK(lock);
static u32 saved[NF_RAILS];
static bool applied, gfx_on, gfx_event_seen, nb_registered;
static struct device *gpu_dev;
static unsigned long gfx_events[4];

static bool pmc_live(void)
{
	u32 v = readl(pmgr + NF_PMGR_PS_PMC);

	return nf_ps_plausible(v) && nf_ps_active(v);
}

static u32 en_rd(unsigned int rail)
{
	return readl(pmc + NF_PMC_IF_ENABLE + 4 * rail);
}

/* Caller holds lock.  Read-modify-write with readback, like writeEnableRegister. */
static int en_update(unsigned int rail, u32 set, u32 clear)
{
	u32 old = en_rd(rail), want = (old | set) & ~clear;

	if (want == old)
		return 0;
	writel(want, pmc + NF_PMC_IF_ENABLE + 4 * rail);
	if (en_rd(rail) != want) {
		pr_err_ratelimited("neo_pmc: rail %u enable %#x -> %#x readback %#x\n", rail, old, want,
				   en_rd(rail));
		return -EIO;
	}
	return 0;
}

static int gfx_set(bool on)
{
	unsigned int r;
	int ret = 0;

	for (r = 0; r < NF_RAILS; r++)
		if (NF_GFX_AGENT_RAILMASK & BIT(r))
			ret = ret ?: en_update(r, on ? BIT(NF_GFX_AGENT_BIT) : 0, on ? 0 : BIT(NF_GFX_AGENT_BIT));
	if (!ret)
		gfx_on = on;
	return ret;
}

/* Atomic context (IRQ-safe genpd, IRQs off): raw spinlock + MMIO only. */
static int genpd_event(struct notifier_block *nb, unsigned long action, void *data)
{
	unsigned long flags;

	raw_spin_lock_irqsave(&lock, flags);
	if (action < ARRAY_SIZE(gfx_events))
		gfx_events[action]++;
	if (action == GENPD_NOTIFY_ON || action == GENPD_NOTIFY_PRE_OFF) {
		gfx_event_seen = true;
		if (applied && pmc_live())
			gfx_set(action == GENPD_NOTIFY_ON);
	}
	raw_spin_unlock_irqrestore(&lock, flags);
	return NOTIFY_OK;
}

static struct notifier_block genpd_nb = { .notifier_call = genpd_event };

/*
 * genpd allows one power notifier per device (pmdomain/core.c: -EEXIST), so
 * with neo_pmpdash loaded the GFX voter is driven from its notifier instead:
 * native _enableDeviceGated issues the PMP device report first, then the PMC
 * voter update, in both directions.  Load neo_pmc with gfx_voter=0 then.
 * Atomic-safe: raw spinlock + MMIO only.
 */
int neo_pmc_gfx_set(bool on)
{
	unsigned long flags;
	int ret = -ENODEV;

	raw_spin_lock_irqsave(&lock, flags);
	if (applied && pmc_live()) {
		gfx_events[on ? GENPD_NOTIFY_ON : GENPD_NOTIFY_PRE_OFF]++;
		ret = gfx_set(on);
	}
	raw_spin_unlock_irqrestore(&lock, flags);
	return ret;
}
EXPORT_SYMBOL_GPL(neo_pmc_gfx_set);

static int status_show(struct seq_file *s, void *unused)
{
	u32 en[NF_RAILS] = {}, st[NF_RAILS] = {}, ps_pmc, ps_gfx;
	unsigned long ev[ARRAY_SIZE(gfx_events)], flags;
	bool live, on, app;
	unsigned int r;

	/* Snapshot under the raw lock; format outside it. */
	raw_spin_lock_irqsave(&lock, flags);
	ps_pmc = readl(pmgr + NF_PMGR_PS_PMC);
	ps_gfx = readl(pmgr + NF_PMGR_PS_GFX);
	live = pmc_live();
	for (r = 0; live && r < NF_RAILS; r++) {
		en[r] = en_rd(r);
		st[r] = readl(pmc + NF_PMC_RAIL_STATE + 4 * r);
	}
	memcpy(ev, gfx_events, sizeof(ev));
	on = gfx_on;
	app = applied;
	raw_spin_unlock_irqrestore(&lock, flags);

	seq_printf(s, "active=%u applied=%u gfx_voter=%u gfx_on=%u notifier=%u ps_pmc=%#x ps_gfx=%#x\n",
		   active, app, gfx_voter, on, nb_registered, ps_pmc, ps_gfx);
	seq_printf(s, "genpd events pre_off=%lu off=%lu pre_on=%lu on=%lu\n", ev[0], ev[1], ev[2], ev[3]);
	for (r = 0; live && r < NF_RAILS; r++)
		seq_printf(s, "rail%u enable=%#x saved=%#x state=%#x\n", r, en[r], saved[r], st[r]);
	return 0;
}
DEFINE_SHOW_ATTRIBUTE(status);

static struct device *find_gpu(void)
{
	struct device_node *np = of_find_compatible_node(NULL, NULL, "apple,agx-t8140");
	struct platform_device *pdev;

	if (!np)
		return NULL;
	pdev = of_find_device_by_node(np);
	of_node_put(np);
	return pdev ? &pdev->dev : NULL;
}

static int __init pmc_init(void)
{
	unsigned long flags;
	unsigned int r, a;
	int ret = 0;

	if (!of_machine_is_compatible("apple,j700"))
		return -ENODEV;
	pmgr = ioremap(NF_PMGR_BASE, 0x4000);
	pmc = ioremap(NF_PMC_BASE, NF_PMC_SIZE);
	if (!pmgr || !pmc) {
		ret = -ENOMEM;
		goto unmap;
	}
	if (!pmc_live()) {
		ret = -EIO;
		goto unmap;
	}
	for (r = 0; r < NF_RAILS; r++)
		saved[r] = en_rd(r);
	dir = debugfs_create_dir("neo_pmc", NULL);
	debugfs_create_file("status", 0400, dir, NULL, &status_fops);
	if (!active) {
		pr_info("neo_pmc: status only\n");
		return 0;
	}
	raw_spin_lock_irqsave(&lock, flags);
	for (r = 0; r < NF_RAILS && !ret; r++) {
		u32 set = 0;

		if (!(RAIL_MASK & BIT(r)))
			continue;
		for (a = 0; a < ARRAY_SIZE(static_agents); a++)
			set |= BIT(static_agents[a]);
		ret = en_update(r, set, 0);
	}
	applied = !ret;
	raw_spin_unlock_irqrestore(&lock, flags);
	if (ret)
		goto restore;
	if (gfx_voter) {
		gpu_dev = find_gpu();
		if (gpu_dev && !dev_pm_genpd_add_notifier(gpu_dev, &genpd_nb)) {
			nb_registered = true;
		} else {
			pr_warn("neo_pmc: no genpd notifier on the GPU device; GFX voter left unchanged\n");
		}
		if (nb_registered) {
			u32 ps;

			/*
			 * Initial state = host target nibble (what the native
			 * requested-state table holds).  Read under the lock and
			 * skipped if a genpd ON/PRE_OFF already arrived, so a
			 * racing PRE_OFF is never overwritten by a stale "on".
			 */
			raw_spin_lock_irqsave(&lock, flags);
			ps = readl(pmgr + NF_PMGR_PS_GFX);
			if (!gfx_event_seen && nf_ps_plausible(ps) && nf_ps_target(ps) == 0xf)
				gfx_set(true);
			raw_spin_unlock_irqrestore(&lock, flags);
		}
	}
	pr_info("neo_pmc: static voters enabled on all rails; gfx tracking %s\n",
		nb_registered ? "on" : "off");
	return 0;
restore:
	raw_spin_lock_irqsave(&lock, flags);
	for (r = 0; r < NF_RAILS; r++)
		writel(saved[r], pmc + NF_PMC_IF_ENABLE + 4 * r);
	applied = false;
	raw_spin_unlock_irqrestore(&lock, flags);
	debugfs_remove_recursive(dir);
unmap:
	if (pmc)
		iounmap(pmc);
	if (pmgr)
		iounmap(pmgr);
	return ret;
}

static void __exit pmc_exit(void)
{
	unsigned long flags;
	unsigned int r;

	/* Process context; after this no notifier can run. */
	if (nb_registered)
		dev_pm_genpd_remove_notifier(gpu_dev);
	if (gpu_dev)
		put_device(gpu_dev);
	debugfs_remove_recursive(dir);
	raw_spin_lock_irqsave(&lock, flags);
	if (applied && pmc_live())
		for (r = 0; r < NF_RAILS; r++)
			writel(saved[r], pmc + NF_PMC_IF_ENABLE + 4 * r);
	applied = false;
	raw_spin_unlock_irqrestore(&lock, flags);
	iounmap(pmc);
	iounmap(pmgr);
}

module_init(pmc_init);
module_exit(pmc_exit);
MODULE_LICENSE("GPL");
MODULE_DESCRIPTION("T8140 PMC voter-interface enables modelled on macOS restorePMC/stateChange");
