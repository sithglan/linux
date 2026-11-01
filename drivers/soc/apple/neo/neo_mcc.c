// SPDX-License-Identifier: GPL-2.0-only
/*
 * neo_mcc: T8140 (J700) system-level-cache allocation owner with the macOS
 * AppleT8140MCC lifecycle.  Production default; opt out with
 * neo_mcc.enable=0 on the kernel command line.
 *
 * Native lifecycle (27.2 kernelcache, com.apple.driver.AppleT8140MCC):
 *  - boot:     MemCacheCIP::start -> mccEnableCacheMode(true) (0x991ffe0):
 *              0x2800 = ways (broadcast), mccWaitForWaysPwrOn, then
 *              0x82c = 0 (broadcast, block 0x9921d94).
 *  - active:   callPlatformFunction(gIOPlatformActiveActionKey) ->
 *              mccEnableCacheMode(true) again (0x9920800).
 *  - sleep:    no cache-off on J700 (ADT mcc skip-mcc-off-to-s2r = 1).
 *  - quiesce:  callPlatformFunction(gIOPlatformQuiesceActionKey) ->
 *              mccEnableCacheMode(false) (0x9920b70) = cacheFlush only:
 *              0x82c = 1, 0x2844 = 1 (both broadcast), poll every plane's
 *              +0x2844 bit 0 clear.  Ways stay enabled.
 *  - panic:    flush only if flush-cache-on-panic (J700 ADT: 0) -> none.
 *  AMCC = ADT mcc reg[2] 0x220000000 (amcc-count 1, 4 planes, stride
 *  0x40000); broadcast window +0x100000.  Native writes go through the SPTM
 *  I/O filter; this boot chain has no SPTM, so they are direct writes.
 *
 * Linux mapping:
 *  - init:     step 3 only when every plane already reports ways enabled ==
 *              ways powered and no flush in flight (the ways are enabled at
 *              handoff on this chain; the 0x2800 write is not repeated);
 *              readback on all planes.
 *  - resume:   PM_POST_SUSPEND re-applies step 3 if 0x82c was set again
 *              (native Active action).  s2idle keeps the AMCC powered.
 *  - reboot/poweroff/halt/kexec: reboot notifier runs the native quiesce
 *              cacheFlush (bounded poll).
 *  - debugfs neo_mcc/status: read-only per-plane state.
 */
#include <linux/debugfs.h>
#include <linux/delay.h>
#include <linux/io.h>
#include <linux/module.h>
#include <linux/mutex.h>
#include <linux/notifier.h>
#include <linux/of.h>
#include <linux/reboot.h>
#include <linux/seq_file.h>
#include <linux/suspend.h>

#define AMCC_BASE	0x220000000ULL
#define AMCC_SIZE	0x140000
#define PLANES		4
#define PLANE_STRIDE	0x40000
#define BROADCAST	0x100000
#define CACHE_WAYS	0x2800
#define WAYS_POWERED	0x2804
#define ALLOC_OFF	0x82c
#define FLUSH		0x2844

static bool enable = true;
module_param(enable, bool, 0444);
MODULE_PARM_DESC(enable, "Enable SLC allocation at load (native default); neo_mcc.enable=0 opts out");
static bool quiesce_flush = true;
module_param(quiesce_flush, bool, 0444);
MODULE_PARM_DESC(quiesce_flush, "Native quiesce cacheFlush on reboot/poweroff/kexec");
static unsigned int flush_timeout_ms = 100;
module_param(flush_timeout_ms, uint, 0444);
MODULE_PARM_DESC(flush_timeout_ms, "Bound on the cacheFlush completion poll (native: none)");

static void __iomem *amcc;
static struct dentry *dir;
static DEFINE_MUTEX(lock);
static int enable_result = -ENODATA, flush_result = -ENODATA;
static unsigned long n_enable, n_resume, n_flush;
static bool active;

static u32 prd(unsigned int p, u32 off)
{
	return readl(amcc + p * PLANE_STRIDE + off);
}

/* lock held.  Native mccEnableCacheMode(true) step 3. */
static int alloc_enable(void)
{
	unsigned int p;

	for (p = 0; p < PLANES; p++) {
		u32 ways = prd(p, CACHE_WAYS) & 0x1f, pw = (prd(p, WAYS_POWERED) >> 24) & 0x7f;

		if (!ways || pw != ways || (prd(p, FLUSH) & 1)) {
			pr_warn("neo_mcc: plane%u ways %u powered %u flush %#x: not enabling allocation\n", p,
				ways, pw, prd(p, FLUSH));
			return -EBUSY;
		}
	}
	writel(0, amcc + BROADCAST + ALLOC_OFF);
	wmb();
	n_enable++;
	for (p = 0; p < PLANES; p++)
		if (prd(p, ALLOC_OFF)) {
			pr_warn("neo_mcc: plane%u 0x82c readback %#x (write ignored)\n", p, prd(p, ALLOC_OFF));
			return -EIO;
		}
	return 0;
}

/* lock held.  Native quiesce: cacheFlush (ways stay enabled). */
static int alloc_flush(void)
{
	unsigned int p, t;

	writel(1, amcc + BROADCAST + ALLOC_OFF);
	writel(1, amcc + BROADCAST + FLUSH);
	wmb();
	n_flush++;
	for (p = 0; p < PLANES; p++) {
		for (t = 0; (prd(p, FLUSH) & 1) && t < flush_timeout_ms * 100; t++)
			udelay(10);
		if (prd(p, FLUSH) & 1)
			return -ETIMEDOUT;
	}
	return 0;
}

static int reboot_event(struct notifier_block *nb, unsigned long action, void *data)
{
	mutex_lock(&lock);
	if (active && quiesce_flush) {
		flush_result = alloc_flush();
		active = false;
		pr_info("neo_mcc: quiesce cacheFlush %d\n", flush_result);
	}
	mutex_unlock(&lock);
	return NOTIFY_DONE;
}

static struct notifier_block reboot_nb = { .notifier_call = reboot_event };

static int pm_event(struct notifier_block *nb, unsigned long action, void *data)
{
	if (action != PM_POST_SUSPEND && action != PM_POST_HIBERNATION)
		return NOTIFY_DONE;
	mutex_lock(&lock);
	if (active && prd(0, ALLOC_OFF)) {
		n_resume++;
		enable_result = alloc_enable();
		pr_info("neo_mcc: resume re-enable %d\n", enable_result);
	}
	mutex_unlock(&lock);
	return NOTIFY_DONE;
}

static struct notifier_block pm_nb = { .notifier_call = pm_event };

static int status_show(struct seq_file *s, void *unused)
{
	unsigned int p;

	mutex_lock(&lock);
	for (p = 0; p < PLANES; p++)
		seq_printf(s, "plane%u 2800=%#010x 2804=%#010x 82c=%#x 2844=%#x\n", p, prd(p, CACHE_WAYS),
			   prd(p, WAYS_POWERED), prd(p, ALLOC_OFF), prd(p, FLUSH));
	seq_printf(s, "enable=%u active=%u enable_result=%d flush_result=%d enables=%lu resumes=%lu flushes=%lu\n",
		   enable, active, enable_result, flush_result, n_enable, n_resume, n_flush);
	mutex_unlock(&lock);
	return 0;
}
DEFINE_SHOW_ATTRIBUTE(status);

/*
 * The default J700 tree hands the allocation word to the apple,t8140-mcc
 * driver (CONFIG_APPLE_MCC). Own it only when no such node is in use, as on
 * the J700 hybrid tree.
 */
static bool mcc_owned_elsewhere(void)
{
	struct device_node *np;
	bool owned;

	if (!IS_ENABLED(CONFIG_APPLE_MCC))
		return false;
	np = of_find_compatible_node(NULL, NULL, "apple,t8140-mcc");
	owned = np && of_device_is_available(np);
	of_node_put(np);
	return owned;
}

static int __init mcc_init(void)
{
	if (!of_machine_is_compatible("apple,j700") || mcc_owned_elsewhere())
		return -ENODEV;
	amcc = ioremap(AMCC_BASE, AMCC_SIZE);
	if (!amcc)
		return -ENOMEM;
	dir = debugfs_create_dir("neo_mcc", NULL);
	debugfs_create_file("status", 0444, dir, NULL, &status_fops);
	if (enable) {
		mutex_lock(&lock);
		enable_result = alloc_enable();
		active = !enable_result;
		mutex_unlock(&lock);
		register_reboot_notifier(&reboot_nb);
		register_pm_notifier(&pm_nb);
	}
	pr_info("neo_mcc: SLC allocation %s (%d)\n", active ? "enabled" : enable ? "NOT enabled" : "opted out",
		enable_result);
	return 0;
}

static void __exit mcc_exit(void)
{
	if (enable) {
		unregister_pm_notifier(&pm_nb);
		unregister_reboot_notifier(&reboot_nb);
	}
	debugfs_remove_recursive(dir);
	/* Allocation stays enabled (native state); disable = reboot or neo_slc_alloc. */
	iounmap(amcc);
}

module_init(mcc_init);
module_exit(mcc_exit);
MODULE_LICENSE("GPL");
MODULE_DESCRIPTION("T8140 SLC allocation owner (native AppleT8140MCC boot/active/quiesce lifecycle)");
