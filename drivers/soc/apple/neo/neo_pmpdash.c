// SPDX-License-Identifier: GPL-2.0-only
/*
 * neo_pmpdash: T8140 (J700) host -> PMP device-state reports over the PMP
 * dashboard (PTD), modelled on macOS ApplePMGR's PMP-interface v2 path.
 *
 * Native evidence (27.2 kernelcache; ADT = live J700 ADT 2026-09-24):
 *  - ADT pmgr "pmp" = 2.  ApplePMGR::_sendPMPCommand (0xfffffe00094f94c4):
 *    with _pmpV2() true, commands 0xc/0xe/0xf are routed to
 *    _pmpWriteDashBoard (0x950be7c); 0xd returns unsupported on v2.
 *    Command 0xe/0xf {device id, state} goes to
 *    _pmpWriteDashBoardSetDeviceState (0x950cc78) for non-virtual devices.
 *  - SetDeviceState: bit = PMP soc-device index of the device (the PMGR
 *    ADT device entry byte 3 is the PMP soc-device id); read the
 *    SOC-DEV-PS-REQ dashboard entry, set the bit (state != 0) or clear it,
 *    and write it back (ApplePTD::_writePTD 0x9534600 = 64-bit write at
 *    +0x10000 + 8*idx).  If the soc-device type has bit 1 set and PMP-STATUS
 *    is non-zero, poll SOC-DEV-PS-ACK until its meta bit (raw word 1 bit 1)
 *    is set and the ack bit equals the request bit (15 s native timeout).
 *  - Dashboard read (ApplePTD::_readPTD 0x9534588): 16-byte records
 *    {u64 value, u64 meta} at base + 16*idx.  Base 0x3003c0000 = ADT mcc
 *    reg[6] (pmp_ptd_db_reg_idx); update window 0x3003d0000 = ADT mcc reg[7]
 *    and pmp reg[3] (ptd-update-reg-index).
 *  - ADT iop-pmp-nub ptd-range: PMP-STATUS idx 1, DVFS-STATE idx 8..11,
 *    SOC-DEV-PS-REQ idx 0x188 (4), SOC-DEV-PS-ACK idx 0x18c (4).
 *    ADT pmgr ptd-ranges = {0xa,0xb,0xc,0xd,0x2,0x4}.
 *  - ADT iop-pmp-nub soc-device: 28 records of 0x7c bytes (the same stride
 *    SetDeviceState uses); AGX is index 9 (id 5, type 3 = ack required).
 *  - _notifyPMPInitialDeviceStatusGated (0x95105ec): at PMP ready, report
 *    every PMP-tracked device (flags bit 1) whose requested state is on.
 *    _enableDeviceGated (0x95108c8): report on after the power-on PS write,
 *    off before the power-off PS write (GFX flags 0x82: bit 6 clear), and
 *    the PMP report precedes the PMC voter update in both directions.
 *    _sendACCStateChangeNotificationGated (0x94f9354): CPU cluster state
 *    (ECPM 0x7 -> EACC0 idx 7, PCPM 0x8 -> PACC0 idx 8, both type 3).
 *
 * Linux mapping:
 *  - active=0: read-only debugfs status (dashboard, PS, requests, acks).
 *  - active=1 (requires PMP RUN): initial report as native, then GFX follows
 *    the gfx genpd (IRQ-safe: raw spinlock + MMIO + bounded ack poll; a
 *    PRE_OFF that the PMP does not ack aborts the power-off), other tracked
 *    devices follow their PMGR PS target by a poll (deviation: native is
 *    event-driven; these need no ack natively).  CPU clusters only with
 *    notify_acc=1.  Unload restores the request word saved at load.
 */
#include <linux/debugfs.h>
#include <linux/delay.h>
#include <linux/io.h>
#include <linux/iopoll.h>
#include <linux/module.h>
#include <linux/of.h>
#include <linux/of_platform.h>
#include <linux/platform_device.h>
#include <linux/pm_domain.h>
#include <linux/seq_file.h>
#include <linux/spinlock.h>
#include <linux/workqueue.h>

/* Exported by neo_pmc (optional; resolved with symbol_get). */
int neo_pmc_gfx_set(bool on);

#define PMGR_BASE	0x300700000ULL
#define PMGR_SIZE	0x4000
#define PMP_ASC_BASE	0x300e00000ULL
#define PMP_CPU_CONTROL	0x44
#define PMP_CPU_RUN	BIT(4)
#define PTD_DB_BASE	0x3003c0000ULL	/* 16-byte records */
#define PTD_UPD_BASE	0x3003d0000ULL	/* 64-bit update words */
#define PTD_WIN_SIZE	0x4000

#define PTD_PMP_STATUS	0x1
#define PTD_DVFS_STATE	0x8	/* 4 entries */
#define PTD_PS_REQ	0x188	/* 4 entries; entry 0 carries all 28 bits */
#define PTD_PS_ACK	0x18c
#define PTD_META_FRESH	BIT_ULL(1)	/* MCC updateDataStreamSize: "new data" */

struct dev_ent {
	const char *name;
	u16 pmgr_id;	/* ADT pmgr devices id */
	u16 ps;		/* PS register offset from 0x300700000 */
	u8 bit;		/* ADT iop-pmp-nub soc-device index */
	u8 type;	/* soc-device +8 (bit 1 = ack) */
	u8 acc;		/* CPU cluster (notify_acc) */
	u8 gfx;
};

/* ADT: pmgr devices (flags bit 1, bit 4 clear) x iop-pmp-nub soc-device. */
static const struct dev_ent devs[] = {
	{ "ECPM",	  0x07, 0x050,  7, 3, 1, 0 },
	{ "PCPM",	  0x08, 0x058,  8, 3, 1, 0 },
	{ "GFX",	  0x28, 0x1f0,  9, 3, 0, 1 },
	{ "JPG",	  0x30, 0x230, 19, 1, 0, 0 },
	{ "MSR",	  0x31, 0x238, 18, 1, 0, 0 },
	{ "VENC1_SYS",	  0x33, 0x248, 16, 1, 0, 0 },
	{ "PRORES",	  0x34, 0x250, 14, 1, 0, 0 },
	{ "ISP_SYS",	  0x37, 0x268, 11, 1, 0, 0 },
	{ "AVD_SYS",	  0x38, 0x270, 17, 1, 0, 0 },
	{ "DISP_SYS",	  0x3a, 0x280, 12, 1, 0, 0 },
	{ "DISPEXT0_SYS", 0x3b, 0x288, 13, 1, 0, 0 },
};

static bool active;
module_param(active, bool, 0444);
MODULE_PARM_DESC(active, "Report device state to a running PMP (default 0: read-only status)");
static bool notify_acc;
module_param(notify_acc, bool, 0444);
MODULE_PARM_DESC(notify_acc, "Also report both CPU clusters on (EACC0/PACC0), as native ACC notifications");
static bool gfx_events = true;
module_param(gfx_events, bool, 0444);
MODULE_PARM_DESC(gfx_events, "GFX follows gfx genpd events (else it is polled like the others)");
static unsigned int poll_ms = 20;
module_param(poll_ms, uint, 0444);
MODULE_PARM_DESC(poll_ms, "PS poll period for non-GFX devices, ms (0 = initial report only)");
static unsigned int ack_ms = 15000;
module_param(ack_ms, uint, 0444);
MODULE_PARM_DESC(ack_ms, "Ack budget, ms (native _pmpWriteDashBoardSetDeviceState: 15 s)");
static bool pmc_gfx = true;
module_param(pmc_gfx, bool, 0444);
MODULE_PARM_DESC(pmc_gfx, "Also drive neo_pmc's GFX voter from this notifier (load neo_pmc active=1 gfx_voter=0 first)");
static unsigned int ack_atomic_us = 100;
module_param(ack_atomic_us, uint, 0444);
MODULE_PARM_DESC(ack_atomic_us, "GFX PRE_OFF: ack wait inside the IRQ-safe genpd notifier before deferring the power-off, us");

static void __iomem *pmgr, *pmp_asc, *db, *upd;
static DEFINE_RAW_SPINLOCK(lock);
static struct dentry *dir;
static struct device *gpu_dev;
static bool nb_registered, applied;
static u64 saved_req;
static u8 reported[ARRAY_SIZE(devs)];	/* 0 off, 1 on, 0xff unknown */
static unsigned long n_writes, n_acks, n_ack_timeouts, n_aborted_off, n_gfx_on, n_gfx_off;
static unsigned long n_ack_fresh, n_ack_changed, n_fresh_seen;
static u64 ack_lat_max_us, ack_lat_last_us;
static struct delayed_work poll_work;
static struct workqueue_struct *wq;
/* neo_pmc's exported GFX voter setter, resolved once at load (optional). */
static int (*pmc_gfx_set)(bool on);
static struct work_struct ack_work;

/* One pending ack per device, protected by lock. */
struct pending {
	bool active;
	bool fresh_seen;
	u64 want;	/* request word written */
	u64 pre_ack;	/* ack word before the write */
	ktime_t t0;
};
static struct pending pend[ARRAY_SIZE(devs)];

/*
 * Dashboard record read in native order: ApplePTD::_readPTD loads the pair
 * with one ldp, value (+0) first, then meta (+8).  Reading +8 on its own can
 * return a stale meta word (lab 2026-09-29: acks "timed out" with ACK == REQ
 * and meta bit 1 set as soon as +0 was read), so always read +0 first.
 */
static u64 ptd_rec(unsigned int idx, u64 *meta)
{
	u64 v = readq(db + 16 * idx);

	*meta = readq(db + 16 * idx + 8);
	return v;
}

static u64 ptd_val(unsigned int idx)
{
	u64 m;

	return ptd_rec(idx, &m);
}

static bool pmp_running(void)
{
	return readl(pmp_asc + PMP_CPU_CONTROL) & PMP_CPU_RUN;
}

static u32 ps_rd(const struct dev_ent *d)
{
	return readl(pmgr + d->ps);
}

static bool ps_target_on(const struct dev_ent *d)
{
	return (ps_rd(d) & 0xf) == 0xf;
}

/*
 * Caller holds lock.  Native SetDeviceState always writes the request word
 * (even when unchanged); the write is what the PMP acknowledges.
 */
static u64 req_update(unsigned int i, bool on)
{
	unsigned int bit = devs[i].bit;
	u64 req = ptd_val(PTD_PS_REQ), want = on ? req | BIT_ULL(bit) : req & ~BIT_ULL(bit);

	pend[i].pre_ack = ptd_val(PTD_PS_ACK);
	pend[i].want = want;
	pend[i].fresh_seen = false;
	pend[i].t0 = ktime_get();
	writeq(want, upd + 8 * PTD_PS_REQ);
	n_writes++;
	reported[i] = on;
	return want;
}

/*
 * Caller holds lock.  Native condition: a fresh ack record (meta bit 1) whose
 * bit equals the request bit.  The fresh flag may be consumed by any read, so
 * a fresh observation is latched, and an ack word that changed since the
 * write is accepted as well (counted separately).
 */
static bool acked(unsigned int i)
{
	u64 meta, v = ptd_rec(PTD_PS_ACK, &meta);
	bool match = !((v ^ pend[i].want) & BIT_ULL(devs[i].bit));

	if (meta & PTD_META_FRESH) {
		pend[i].fresh_seen = true;
		n_fresh_seen++;
	}
	if (!match)
		return false;
	if (pend[i].fresh_seen)
		n_ack_fresh++;
	else if (v != pend[i].pre_ack)
		n_ack_changed++;
	else
		return false;
	return true;
}

static void ack_done(unsigned int i)
{
	u64 us = ktime_us_delta(ktime_get(), pend[i].t0);

	pend[i].active = false;
	n_acks++;
	ack_lat_last_us = us;
	if (us > ack_lat_max_us)
		ack_lat_max_us = us;
}

/* Native: no ack wait unless the type needs one and PMP-STATUS is non-zero. */
static bool needs_ack(const struct dev_ent *d)
{
	return (d->type & 2) && ptd_val(PTD_PMP_STATUS);
}

/* Process context: write, then wait (native: synchronous, 15 s budget). */
static int report(unsigned int i, bool on)
{
	const struct dev_ent *d = &devs[i];
	unsigned long flags;
	bool ok = false;
	u64 meta, ack;

	raw_spin_lock_irqsave(&lock, flags);
	req_update(i, on);
	pend[i].active = needs_ack(d);
	raw_spin_unlock_irqrestore(&lock, flags);
	while (pend[i].active) {
		raw_spin_lock_irqsave(&lock, flags);
		ok = acked(i);
		if (ok)
			ack_done(i);
		raw_spin_unlock_irqrestore(&lock, flags);
		if (ok)
			break;
		if (ktime_us_delta(ktime_get(), pend[i].t0) > (s64)ack_ms * 1000) {
			pend[i].active = false;
			n_ack_timeouts++;
			ack = ptd_rec(PTD_PS_ACK, &meta);
			pr_warn("neo_pmpdash: %s %s not acked in %u ms (req %#llx ack %#llx meta %#llx)\n",
				d->name, on ? "on" : "off", ack_ms, pend[i].want, ack, meta);
			return -ETIMEDOUT;
		}
		usleep_range(100, 200);
	}
	return 0;
}

/* Completes acks started in atomic context (GFX ON, deferred GFX OFF). */
static void ack_fn(struct work_struct *w)
{
	unsigned long flags;
	unsigned int i;
	bool any;
	u64 meta, ack;

	do {
		any = false;
		raw_spin_lock_irqsave(&lock, flags);
		for (i = 0; i < ARRAY_SIZE(devs); i++) {
			if (!pend[i].active)
				continue;
			if (acked(i)) {
				ack_done(i);
			} else if (ktime_us_delta(ktime_get(), pend[i].t0) > (s64)ack_ms * 1000) {
				pend[i].active = false;
				n_ack_timeouts++;
				ack = ptd_rec(PTD_PS_ACK, &meta);
				pr_warn_ratelimited("neo_pmpdash: %s not acked in %u ms (req %#llx ack %#llx meta %#llx)\n",
						    devs[i].name, ack_ms, pend[i].want, ack, meta);
			} else {
				any = true;
			}
		}
		raw_spin_unlock_irqrestore(&lock, flags);
		if (any)
			usleep_range(200, 500);
	} while (any);
}

static int gfx_index(void)
{
	unsigned int i;

	for (i = 0; i < ARRAY_SIZE(devs); i++)
		if (devs[i].gfx)
			return i;
	return -1;
}

/*
 * IRQ-safe genpd notifier (raw spinlock + MMIO only; no multi-second wait in
 * atomic context).  Native _enableDeviceGated waits synchronously for the PMP
 * ack; on Linux:
 *  - ON (after power-on): write the request, then return; ack_work completes
 *    the ack.  The GPU may start before the PMP acks -- the ack only confirms
 *    the PMP's own bookkeeping, it gates nothing on the GPU side.
 *  - PRE_OFF (before power-off): write, then wait at most ack_atomic_us.
 *    Native never powers AGX off before the PMP acks, so if it has not acked
 *    the power-off is refused (genpd keeps the domain on and retries on the
 *    next idle), the request is restored to "on", and ack_work re-acks it.
 * Priority 10 runs this before neo_pmc's GFX voter, the native order.
 */
static int genpd_event(struct notifier_block *nb, unsigned long action, void *data)
{
	int g = gfx_index();
	unsigned long flags;
	unsigned int t;
	bool on;

	if (g < 0 || (action != GENPD_NOTIFY_ON && action != GENPD_NOTIFY_PRE_OFF))
		return NOTIFY_DONE;
	on = action == GENPD_NOTIFY_ON;
	raw_spin_lock_irqsave(&lock, flags);
	if (!applied || !pmp_running()) {
		raw_spin_unlock_irqrestore(&lock, flags);
		return NOTIFY_DONE;
	}
	req_update(g, on);
	if (on)
		n_gfx_on++;
	else
		n_gfx_off++;
	if (!needs_ack(&devs[g])) {
		pend[g].active = false;
		raw_spin_unlock_irqrestore(&lock, flags);
		if (pmc_gfx_set)
			pmc_gfx_set(on);
		return NOTIFY_OK;
	}
	if (on) {
		pend[g].active = true;
		raw_spin_unlock_irqrestore(&lock, flags);
		queue_work(wq, &ack_work);
		if (pmc_gfx_set)
			pmc_gfx_set(true);	/* native: PMP report, then PMC voter */
		return NOTIFY_OK;
	}
	for (t = 0; t < ack_atomic_us; t += 2) {
		if (acked(g))
			break;
		udelay(2);
	}
	if (t < ack_atomic_us) {
		ack_done(g);
		raw_spin_unlock_irqrestore(&lock, flags);
		if (pmc_gfx_set)
			pmc_gfx_set(false);	/* native: PMP report, then PMC voter */
		return NOTIFY_OK;
	}
	req_update(g, true);
	pend[g].active = true;
	n_aborted_off++;
	raw_spin_unlock_irqrestore(&lock, flags);
	queue_work(wq, &ack_work);
	return notifier_from_errno(-EBUSY);
}

static struct notifier_block genpd_nb = { .notifier_call = genpd_event, .priority = 10 };

static void poll_fn(struct work_struct *w)
{
	unsigned int i;

	for (i = 0; i < ARRAY_SIZE(devs); i++) {
		const struct dev_ent *d = &devs[i];
		bool on;

		if (d->acc || (d->gfx && nb_registered))
			continue;
		on = ps_target_on(d);
		if (reported[i] != on)
			report(i, on);
	}
	if (poll_ms)
		queue_delayed_work(wq, &poll_work, msecs_to_jiffies(poll_ms));
}

static int status_show(struct seq_file *s, void *unused)
{
	unsigned int i;
	u64 v, m, v2, m2;

	seq_printf(s, "active=%u applied=%u notify_acc=%u gfx_notifier=%u poll_ms=%u pmp_run=%u\n",
		   active, applied, notify_acc, nb_registered, poll_ms, pmp_running());

	v = ptd_rec(PTD_PMP_STATUS, &m);
	seq_printf(s, "PMP-STATUS[1] val=%#llx meta=%#llx\n", v, m);
	/* DVFS-STATE: host->PMP DVFS override vectors (ApplePMPv2::setDVFSStates), 0 = none. */
	for (i = 0; i < 4; i++) {
		v = ptd_rec(PTD_DVFS_STATE + i, &m);
		seq_printf(s, "DVFS-STATE[%#x] val=%#llx meta=%#llx\n", PTD_DVFS_STATE + i, v, m);
	}
	for (i = 0; i < 4; i++) {
		v = ptd_rec(PTD_PS_REQ + i, &m);
		v2 = ptd_rec(PTD_PS_ACK + i, &m2);
		seq_printf(s, "PS-REQ[%#x] %#llx meta %#llx  PS-ACK[%#x] %#llx meta %#llx\n",
			   PTD_PS_REQ + i, v, m, PTD_PS_ACK + i, v2, m2);
	}
	seq_printf(s, "saved_req=%#llx writes=%lu acks=%lu (fresh %lu, changed %lu, fresh_seen %lu) ack_timeouts=%lu\n",
		   saved_req, n_writes, n_acks, n_ack_fresh, n_ack_changed, n_fresh_seen, n_ack_timeouts);
	seq_printf(s, "ack_latency_us last=%llu max=%llu aborted_off=%lu gfx_on=%lu gfx_off=%lu\n",
		   ack_lat_last_us, ack_lat_max_us, n_aborted_off, n_gfx_on, n_gfx_off);
	for (i = 0; i < ARRAY_SIZE(devs); i++)
		seq_printf(s, "%-13s id=%#04x bit=%2u type=%u ps=%#010x reported=%s\n", devs[i].name,
			   devs[i].pmgr_id, devs[i].bit, devs[i].type, ps_rd(&devs[i]),
			   reported[i] == 0xff ? "-" : reported[i] ? "on" : "off");
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

static void unmap_all(void)
{
	if (upd)
		iounmap(upd);
	if (db)
		iounmap(db);
	if (pmp_asc)
		iounmap(pmp_asc);
	if (pmgr)
		iounmap(pmgr);
}

static int __init pmpdash_init(void)
{
	unsigned int i;
	int ret = 0;

	if (!of_machine_is_compatible("apple,j700"))
		return -ENODEV;
	pmgr = ioremap(PMGR_BASE, PMGR_SIZE);
	pmp_asc = ioremap(PMP_ASC_BASE, 0x1000);
	db = ioremap(PTD_DB_BASE, PTD_WIN_SIZE);
	upd = ioremap(PTD_UPD_BASE, PTD_WIN_SIZE);
	if (!pmgr || !pmp_asc || !db || !upd) {
		unmap_all();
		return -ENOMEM;
	}
	memset(reported, 0xff, sizeof(reported));
	wq = alloc_ordered_workqueue("neo_pmpdash", 0);
	if (!wq) {
		unmap_all();
		return -ENOMEM;
	}
	INIT_DELAYED_WORK(&poll_work, poll_fn);
	INIT_WORK(&ack_work, ack_fn);
	dir = debugfs_create_dir("neo_pmpdash", NULL);
	debugfs_create_file("status", 0400, dir, NULL, &status_fops);
	if (!active) {
		pr_info("neo_pmpdash: read-only\n");
		return 0;
	}
	if (!pmp_running()) {
		pr_err("neo_pmpdash: PMP not running; refusing to report\n");
		ret = -ENODEV;
		goto fail;
	}
	saved_req = ptd_val(PTD_PS_REQ);
	applied = true;
	/* Initial report in PMGR device-id order, as _notifyPMPInitialDeviceStatusGated. */
	for (i = 0; i < ARRAY_SIZE(devs); i++) {
		if (devs[i].acc && !notify_acc)
			continue;
		/* CPU clusters are running while this code runs. */
		if (devs[i].acc || ps_target_on(&devs[i]))
			report(i, true);
		else
			reported[i] = 0;
	}
	if (pmc_gfx)
		pmc_gfx_set = symbol_get(neo_pmc_gfx_set);
	if (pmc_gfx_set) {
		int g = gfx_index();

		if (g >= 0 && reported[g] == 1)
			pmc_gfx_set(true);
	}
	if (gfx_events) {
		gpu_dev = find_gpu();
		if (gpu_dev && !dev_pm_genpd_add_notifier(gpu_dev, &genpd_nb))
			nb_registered = true;
		else
			pr_warn("neo_pmpdash: no gfx genpd notifier; GFX is polled\n");
	}
	if (poll_ms)
		queue_delayed_work(wq, &poll_work, msecs_to_jiffies(poll_ms));
	pr_info("neo_pmpdash: reporting (req %#llx -> %#llx, PMP-STATUS %#llx, acks %lu, timeouts %lu)\n",
		saved_req, ptd_val(PTD_PS_REQ), ptd_val(PTD_PMP_STATUS), n_acks, n_ack_timeouts);
	return 0;
fail:
	debugfs_remove_recursive(dir);
	destroy_workqueue(wq);
	unmap_all();
	return ret;
}

static void __exit pmpdash_exit(void)
{
	unsigned int i;

	cancel_delayed_work_sync(&poll_work);
	if (nb_registered)
		dev_pm_genpd_remove_notifier(gpu_dev);
	cancel_work_sync(&ack_work);
	if (pmc_gfx_set) {
		symbol_put(neo_pmc_gfx_set);
		pmc_gfx_set = NULL;
	}
	if (gpu_dev)
		put_device(gpu_dev);
	debugfs_remove_recursive(dir);
	if (applied && pmp_running()) {
		/* Restore the saved request bits one device at a time, with acks. */
		for (i = 0; i < ARRAY_SIZE(devs); i++) {
			bool was = saved_req & BIT_ULL(devs[i].bit);

			if (reported[i] != 0xff && reported[i] != was)
				report(i, was);
		}
	}
	applied = false;
	destroy_workqueue(wq);
	unmap_all();
}

module_init(pmpdash_init);
module_exit(pmpdash_exit);
MODULE_LICENSE("GPL");
MODULE_DESCRIPTION("T8140 host->PMP device-state dashboard reports modelled on macOS ApplePMGR (PMP v2)");
