// SPDX-License-Identifier: GPL-2.0-only
/*
 * Experimental T8140 ANE platform driver. Optional child power and CE
 * initialization do not start firmware or implement command execution.
 *
 * J700 ADT /arm-io/ane reg[0] describes a 32 MiB cluster, including its DARTs.
 * Do not reserve or ioremap the whole range. Optional power diagnostics use
 * the existing PMGR syscon; other ADT entries include shared CPU windows.
 */
#include <linux/atomic.h>
#include <linux/device.h>
#include <linux/err.h>
#include <linux/io.h>
#include <linux/iopoll.h>
#include <linux/mfd/syscon.h>
#include <linux/module.h>
#include <linux/of.h>
#include <linux/of_address.h>
#include <linux/of_platform.h>
#include <linux/platform_device.h>
#include <linux/pm_domain.h>
#include <linux/pm_runtime.h>
#include <linux/regmap.h>
#include <linux/slab.h>

#include "apple-ane-bringup.h"

#define T8140_ANE_CLUSTER_BASE	0x400000000ULL
#define T8140_ANE_CLUSTER_SIZE	0x2000000ULL
#define T8140_PMGR_BASE		0x300700000ULL
#define T8140_PMGR_SIZE		0x14000ULL
#define T8140_ANE_SYS_PS		0x290
#define T8140_ANE_CPU_PS		0xc000
#define T8140_ANE_SUBBLOCK_PS	0xc008
#define T8140_ANE_PWGATE_BASE	0x300726680ULL
#define T8140_ANE_CE_BASE		0x4018a4000ULL
#define ANE_PWGATE_REQUEST	0xc
#define ANE_PWGATE_ACK		0x30
#define ANE_POLL_US		500000

static bool enable_power_snapshot;
module_param(enable_power_snapshot, bool, 0400);
MODULE_PARM_DESC(enable_power_snapshot, "Allow read-only T8140 PMGR power snapshots");

static bool enable_engine_power;
module_param(enable_engine_power, bool, 0400);
MODULE_PARM_DESC(enable_engine_power, "Enable experimental T8140 child power and CE initialization");

struct apple_ane {
	struct resource cluster;
	struct regmap *pmgr;
	void __iomem *pwgate;
	void __iomem *ce;
	bool children_owned;
	atomic64_t ce_initializations;
	atomic64_t resumes;
	atomic64_t suspends;
};

static ssize_t bringup_status_show(struct device *dev,
				  struct device_attribute *attr, char *buf)
{
	return sysfs_emit(buf, "%s\n", enable_engine_power ?
			 "engine-power-and-ce" : "power-domain-only");
}
static DEVICE_ATTR_RO(bringup_status);

static ssize_t inference_available_show(struct device *dev,
				       struct device_attribute *attr, char *buf)
{
	return sysfs_emit(buf, "0\n");
}
static DEVICE_ATTR_RO(inference_available);

static ssize_t cluster_range_show(struct device *dev,
				  struct device_attribute *attr, char *buf)
{
	struct apple_ane *ane = dev_get_drvdata(dev);

	return sysfs_emit(buf, "%#llx %#llx\n",
			 (unsigned long long)ane->cluster.start,
			 (unsigned long long)resource_size(&ane->cluster));
}
static DEVICE_ATTR_RO(cluster_range);

static ssize_t runtime_transitions_show(struct device *dev,
				       struct device_attribute *attr, char *buf)
{
	struct apple_ane *ane = dev_get_drvdata(dev);

	/* Driver callbacks, not a count of completed ANE operations. */
	return sysfs_emit(buf, "resume=%lld suspend=%lld\n",
			 atomic64_read(&ane->resumes),
			 atomic64_read(&ane->suspends));
}
static DEVICE_ATTR_RO(runtime_transitions);

static ssize_t power_state_show(struct device *dev,
				struct device_attribute *attr, char *buf)
{
	struct apple_ane *ane = dev_get_drvdata(dev);
	unsigned int sys, cpu, subblocks;
	int ret;

	if (!enable_power_snapshot)
		return -EOPNOTSUPP;
	/* PMGR is always accessible; do not wake or touch the ANE aperture. */
	ret = regmap_read(ane->pmgr, T8140_ANE_SYS_PS, &sys);
	if (ret)
		return ret;
	ret = regmap_read(ane->pmgr, T8140_ANE_CPU_PS, &cpu);
	if (ret)
		return ret;
	ret = regmap_read(ane->pmgr, T8140_ANE_SUBBLOCK_PS, &subblocks);
	if (ret)
		return ret;

	/* Sequential observations, not an atomic snapshot of three domains. */
	return sysfs_emit(buf, "sys=0x%08x cpu=0x%08x subblocks=0x%08x\n",
			 sys, cpu, subblocks);
}
static DEVICE_ATTR_RO(power_state);

static ssize_t ce_initializations_show(struct device *dev,
				      struct device_attribute *attr, char *buf)
{
	struct apple_ane *ane = dev_get_drvdata(dev);

	/* Count completed handshakes without waking or reading the engine. */
	return sysfs_emit(buf, "%lld\n", atomic64_read(&ane->ce_initializations));
}
static DEVICE_ATTR_RO(ce_initializations);

static struct attribute *apple_ane_attrs[] = {
	&dev_attr_bringup_status.attr,
	&dev_attr_inference_available.attr,
	&dev_attr_cluster_range.attr,
	&dev_attr_runtime_transitions.attr,
	&dev_attr_power_state.attr,
	&dev_attr_ce_initializations.attr,
	NULL,
};
ATTRIBUTE_GROUPS(apple_ane);

static int apple_ane_set_child(struct apple_ane *ane, unsigned int offset,
			       unsigned int request, unsigned int mask,
			       unsigned int expected)
{
	unsigned int value;
	int ret;

	ret = regmap_write(ane->pmgr, offset, request);
	if (ret)
		return ret;
	return regmap_read_poll_timeout(ane->pmgr, offset, value,
				       (value & mask) == expected, 10, ANE_POLL_US);
}

static int apple_ane_engine_off(struct apple_ane *ane)
{
	u32 value;
	int ret;

	if (!ane->children_owned)
		return 0;
	ret = apple_ane_set_child(ane, T8140_ANE_SUBBLOCK_PS, 0x300, 0x8ff, 0);
	if (ret)
		return ret;
	writel(3, ane->pwgate + ANE_PWGATE_REQUEST);
	ret = readl_poll_timeout(ane->pwgate + ANE_PWGATE_REQUEST, value,
				 (value & 3) == 3, 10, ANE_POLL_US);
	if (ret)
		return ret;
	writel(0, ane->pwgate + ANE_PWGATE_ACK);
	ret = readl_poll_timeout(ane->pwgate + ANE_PWGATE_ACK, value,
				 !(value & 1), 10, ANE_POLL_US);
	if (ret)
		return ret;
	ret = apple_ane_set_child(ane, T8140_ANE_CPU_PS, 0x300, 0x8ff, 0);
	if (!ret)
		ane->children_owned = false;
	return ret;
}

static int apple_ane_engine_on(struct apple_ane *ane)
{
	unsigned int cpu, subblocks;
	u64 wanted, completion;
	u32 value;
	int ret;

	ret = regmap_read(ane->pmgr, T8140_ANE_CPU_PS, &cpu);
	if (ret)
		return ret;
	ret = regmap_read(ane->pmgr, T8140_ANE_SUBBLOCK_PS, &subblocks);
	if (ret)
		return ret;
	/* Refuse active, reset, or automatic child power management. */
	if ((cpu | subblocks) & 0x90000cff ||
	    readl(ane->pwgate + ANE_PWGATE_REQUEST) != 3 ||
	    readl(ane->pwgate + ANE_PWGATE_ACK))
		return -EBUSY;
	ane->children_owned = true;
	ret = apple_ane_set_child(ane, T8140_ANE_CPU_PS, 0xf, 0xff, 0xff);
	if (ret)
		return ret;
	writel(0, ane->pwgate + ANE_PWGATE_REQUEST);
	ret = readl_poll_timeout(ane->pwgate + ANE_PWGATE_REQUEST, value,
				 !(value & 3), 10, ANE_POLL_US);
	if (ret)
		return ret;
	ret = apple_ane_set_child(ane, T8140_ANE_SUBBLOCK_PS, 0xf, 0xff, 0xff);
	if (ret)
		return ret;
	/* H17 CE must complete before any firmware scratch register writes. */
	wanted = (readq(ane->ce + 0x10) & 3) ^ 3;
	if (readq(ane->ce + 0x18) & wanted)
		return -EIO;
	if (wanted) {
		writeq(wanted, ane->ce);
		ret = readq_poll_timeout(ane->ce + 8, completion,
					 (completion & 3) == wanted, 10, 100000);
		if (ret)
			return ret;
		if (readq(ane->ce + 0x18) & wanted)
			return -EIO;
		writeq(wanted, ane->ce + 8);
	}
	if ((readq(ane->ce + 0x10) & 3) != 3)
		return -EIO;
	atomic64_inc(&ane->ce_initializations);
	return 0;
}

static int apple_ane_runtime_resume(struct device *dev)
{
	struct apple_ane *ane = dev_get_drvdata(dev);
	int ret, cleanup;

	/* genpd enables ANE_SYS before invoking this callback. */
	if (enable_engine_power) {
		ret = apple_ane_engine_on(ane);
		if (ret) {
			cleanup = apple_ane_engine_off(ane);
			if (cleanup)
				dev_err(dev, "child power unwind failed: %d\n", cleanup);
			return ret;
		}
	}
	atomic64_inc(&ane->resumes);
	return 0;
}

static int apple_ane_runtime_suspend(struct device *dev)
{
	struct apple_ane *ane = dev_get_drvdata(dev);
	int ret;

	ret = apple_ane_engine_off(ane);
	if (ret)
		return ret;
	atomic64_inc(&ane->suspends);
	return 0;
}

static const struct dev_pm_ops apple_ane_pm_ops = {
	RUNTIME_PM_OPS(apple_ane_runtime_suspend, apple_ane_runtime_resume, NULL)
	SYSTEM_SLEEP_PM_OPS(pm_runtime_force_suspend, pm_runtime_force_resume)
};

static int apple_ane_attach_domain(struct device *dev)
{
	struct apple_ane *ane = dev_get_drvdata(dev);
	struct of_phandle_args args = {};
	struct resource pmgr_res;
	const char *path;
	const char *label;
	u32 reg[2];
	int ret;

	/*
	 * Temporary bring-up binding: the boot DT has no phandle on this unused
	 * provider. Resolve the verified node without rewriting the live tree.
	 * Replace the path property with power-domains in a finalized binding.
	 */
	if (of_property_present(dev->of_node, "power-domains") ||
	    of_property_read_string(dev->of_node, "apple,power-domain-path", &path) ||
	    strcmp(path, "/soc/power-management@300700000/power-controller@290"))
		return -EINVAL;
	args.np = of_find_node_by_path(path);
	if (!args.np)
		return -ENODEV;
	ret = -EINVAL;
	if (!args.args_count &&
	    of_device_is_compatible(args.np, "apple,t8140-pmgr-pwrstate") &&
	    !of_property_read_string(args.np, "label", &label) &&
	    !strcmp(label, "ane_sys") &&
	    !of_property_read_u32_array(args.np, "reg", reg, 2) &&
	    reg[0] == T8140_ANE_SYS_PS && reg[1] == 4) {
		if (enable_power_snapshot || enable_engine_power) {
			if (!of_device_is_compatible(args.np->parent, "apple,t8140-pmgr") ||
			    of_address_to_resource(args.np->parent, 0, &pmgr_res) ||
			    pmgr_res.start != T8140_PMGR_BASE ||
			    resource_size(&pmgr_res) != T8140_PMGR_SIZE)
				goto out;
			ane->pmgr = syscon_node_to_regmap(args.np->parent);
			if (IS_ERR(ane->pmgr)) {
				ret = PTR_ERR(ane->pmgr);
				goto out;
			}
		}
		ret = of_genpd_add_device(&args, dev);
	}
out:
	of_node_put(args.np);
	return ret;
}

static void apple_ane_detach_domain(void *data)
{
	struct device *dev = data;

	int ret;

	/* Force suspension also handles removal while power/control is "on". */
	ret = pm_runtime_force_suspend(dev);
	if (ret) {
		dev_warn(dev, "ANE_SYS suspend during removal failed: %d\n", ret);
		pm_runtime_disable(dev);
	}
	ret = pm_genpd_remove_device(dev);
	if (ret)
		dev_warn(dev, "ANE_SYS detach failed: %d\n", ret);
	pm_runtime_set_suspended(dev);
}

static int apple_ane_probe(struct platform_device *pdev)
{
	struct device *dev = &pdev->dev;
	struct resource *res;
	struct resource *pwgate_res, *ce_res;
	struct apple_ane *ane;
	int ret;

	if (!of_machine_is_compatible("apple,j700") ||
	    !of_machine_is_compatible("apple,t8140"))
		return -ENODEV;

	res = platform_get_resource_byname(pdev, IORESOURCE_MEM, "cluster");
	if (!res || res->start != T8140_ANE_CLUSTER_BASE ||
	    res->end != T8140_ANE_CLUSTER_BASE + T8140_ANE_CLUSTER_SIZE - 1 ||
	    platform_get_resource(pdev, IORESOURCE_MEM, 3))
		return dev_err_probe(dev, -EINVAL, "unexpected T8140 ANE cluster resource\n");
	pwgate_res = platform_get_resource_byname(pdev, IORESOURCE_MEM, "pwgate");
	if (!pwgate_res || pwgate_res->start != T8140_ANE_PWGATE_BASE ||
	    resource_size(pwgate_res) != 0x40)
		return dev_err_probe(dev, -EINVAL, "unexpected T8140 ANE PWGATE resource\n");
	ce_res = platform_get_resource_byname(pdev, IORESOURCE_MEM, "ce");
	if (!ce_res || ce_res->start != T8140_ANE_CE_BASE || resource_size(ce_res) != 0x20)
		return dev_err_probe(dev, -EINVAL, "unexpected T8140 ANE CE resource\n");

	ane = devm_kzalloc(dev, sizeof(*ane), GFP_KERNEL);
	if (!ane)
		return -ENOMEM;
	ane->cluster = *res;
	platform_set_drvdata(pdev, ane);
	if (enable_engine_power) {
		ane->pwgate = devm_ioremap_resource(dev, pwgate_res);
		if (IS_ERR(ane->pwgate))
			return PTR_ERR(ane->pwgate);
		ane->ce = devm_ioremap_resource(dev, ce_res);
		if (IS_ERR(ane->ce))
			return PTR_ERR(ane->ce);
	}

	ret = apple_ane_attach_domain(dev);
	if (ret)
		return dev_err_probe(dev, ret, "expected T8140 ANE_SYS power domain\n");
	/* of_genpd_add_device() attaches without powering up the domain. */
	pm_runtime_enable(dev);
	ret = devm_add_action_or_reset(dev, apple_ane_detach_domain, dev);
	if (ret)
		return ret;
	dev_info(dev, "ANE power bring-up ready (child power %s); inference unavailable\n",
		 enable_engine_power ? "enabled" : "disabled");
	return 0;
}

static const struct of_device_id apple_ane_of_match[] = {
	{ .compatible = "apple,t8140-ane-bringup" },
	{ }
};
MODULE_DEVICE_TABLE(of, apple_ane_of_match);

static struct platform_driver apple_ane_driver = {
	.probe = apple_ane_probe,
	.driver = {
		.name = "apple-ane-bringup",
		.of_match_table = apple_ane_of_match,
		.pm = pm_ptr(&apple_ane_pm_ops),
		.dev_groups = apple_ane_groups,
	},
};

int apple_ane_bringup_get_engine(struct device **held)
{
	struct platform_device *pdev;
	struct device_node *node;
	int ret = -ENODEV;

	if (!held || !enable_engine_power)
		return -EOPNOTSUPP;
	*held = NULL;
	node = of_find_node_by_path("/soc/ane@400000000");
	if (!node)
		return -ENODEV;
	pdev = of_find_device_by_node(node);
	of_node_put(node);
	if (!pdev)
		return -ENODEV;
	device_lock(&pdev->dev);
	if (pdev->dev.driver == &apple_ane_driver.driver)
		ret = pm_runtime_resume_and_get(&pdev->dev);
	device_unlock(&pdev->dev);
	if (ret < 0) {
		put_device(&pdev->dev);
		return ret;
	}
	*held = &pdev->dev;
	return 0;
}
EXPORT_SYMBOL_GPL(apple_ane_bringup_get_engine);

void apple_ane_bringup_put_engine(struct device *held)
{
	if (!held)
		return;
	pm_runtime_put_sync(held);
	put_device(held);
}
EXPORT_SYMBOL_GPL(apple_ane_bringup_put_engine);

module_platform_driver(apple_ane_driver);

MODULE_DESCRIPTION("Apple T8140 ANE resource and power-domain bring-up");
MODULE_LICENSE("GPL");
