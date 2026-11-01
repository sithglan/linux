// SPDX-License-Identifier: GPL-2.0-only
/*
 * Power-path debugging for the Apple ISP
 *
 * With pm_debug set, the driver logs power transitions with the state of the
 * ISP power domains as the PMGR reports them, the runtime PM state of
 * the ISP and its DARTs and, while the ISP CPU is powered, the
 * coprocessor control and handshake words. Before significant transitions
 * it waits pm_debug_delay_ms, so that a log follower can get the
 * line to storage before the step runs.
 *
 * Copyright The Asahi Linux Contributors
 */

#include <linux/debugfs.h>
#include <linux/delay.h>
#include <linux/mfd/syscon.h>
#include <linux/moduleparam.h>
#include <linux/of.h>
#include <linux/of_platform.h>
#include <linux/pm_runtime.h>
#include <linux/regmap.h>
#include <linux/seq_buf.h>
#include <linux/seq_file.h>
#include <linux/slab.h>
#include <linux/string.h>
#include <linux/uaccess.h>

#include "isp-cam.h"
#include "isp-debug.h"
#include "isp-fw.h"
#include "isp-regs.h"

unsigned int apple_isp_pm_debug;
module_param_named(pm_debug, apple_isp_pm_debug, uint, 0644);
MODULE_PARM_DESC(pm_debug, "Log the ISP power path (0 = off)");

static unsigned int apple_isp_pm_debug_delay_ms;
module_param_named(pm_debug_delay_ms, apple_isp_pm_debug_delay_ms, uint,
		   0644);
MODULE_PARM_DESC(pm_debug_delay_ms,
		 "With pm_debug, wait this long before each step that can hang");

#define ISP_DEBUG_LINE 768

static const char *isp_rpm_status(struct device *dev)
{
	switch (dev->power.runtime_status) {
	case RPM_ACTIVE:
		return "active";
	case RPM_RESUMING:
		return "resuming";
	case RPM_SUSPENDED:
		return "suspended";
	case RPM_SUSPENDING:
		return "suspending";
	default:
		return "invalid";
	}
}

static void isp_debug_state(struct apple_isp *isp, struct seq_buf *s,
			    const char *sep)
{
	static const char *const fw_states[] = {
		[ISP_FW_OFF] = "off",
		[ISP_FW_RUNNING] = "running",
		[ISP_FW_DEAD] = "dead",
	};
	u32 val;

	seq_buf_printf(s, "fw %s rpm %s/%d local-on %d", fw_states[isp->fw_state],
		       isp_rpm_status(isp->dev),
		       atomic_read(&isp->dev->power.usage_count), isp->pds_on);

	/* The PMGR is always on; this reads what the hardware reports. */
	seq_buf_printf(s, "%sps", sep);
	for (int i = 0; i < isp->pd_count; i++) {
		if (!isp->dbg.pd_regmap || !isp->dbg.pd_regmap[i] ||
		    regmap_read(isp->dbg.pd_regmap[i], isp->dbg.pd_offset[i],
				&val)) {
			seq_buf_printf(s, " %d=?", i);
			continue;
		}
		seq_buf_printf(s, " %s=%08x", isp->dbg.pd_name[i], val);
	}

	seq_buf_printf(s, "%sdart", sep);
	for (int i = 0; i < isp->dbg.num_darts; i++) {
		struct device *dart = isp->dbg.darts[i];

		seq_buf_printf(s, " %s=%s/%d", dev_name(dart),
			       isp_rpm_status(dart),
			       atomic_read(&dart->power.usage_count));
	}

	/*
	 * The coprocessor and the handshake words are only read while the
	 * ISP and its CPU domain are held on.
	 */
	if (isp->pds_on > 0 && isp->dev->power.runtime_status == RPM_ACTIVE) {
		u32 ctl = isp->hw->coproc_control ?: ISP_COPROC_CONTROL;

		seq_buf_printf(s, "%sctl %08x gpio0 %08x gpio3 %08x gpio7 %08x",
			       sep, readl(isp->coproc + ctl),
			       readl(isp->gpio + ISP_GPIO_0),
			       readl(isp->gpio + ISP_GPIO_3),
			       readl(isp->gpio + ISP_GPIO_7));
	}
}

void __apple_isp_pm_trace(struct apple_isp *isp, bool step, const char *fmt,
			  ...)
{
	char *buf __free(kfree) = kmalloc(ISP_DEBUG_LINE, GFP_KERNEL);
	struct va_format vaf;
	struct seq_buf s;
	va_list args;

	if (!buf)
		return;

	seq_buf_init(&s, buf, ISP_DEBUG_LINE);
	va_start(args, fmt);
	vaf.fmt = fmt;
	vaf.va = &args;
	seq_buf_printf(&s, "%pV", &vaf);
	va_end(args);
	seq_buf_puts(&s, " | ");
	isp_debug_state(isp, &s, " | ");

	dev_info(isp->dev, "pm: %s%s\n", step ? "step " : "", seq_buf_str(&s));

	if (step && READ_ONCE(apple_isp_pm_debug_delay_ms))
		msleep(READ_ONCE(apple_isp_pm_debug_delay_ms));
}

static int isp_debug_power_show(struct seq_file *m, void *data)
{
	struct apple_isp *isp = m->private;
	char *buf __free(kfree) = kmalloc(ISP_DEBUG_LINE, GFP_KERNEL);
	struct seq_buf s;

	if (!buf)
		return -ENOMEM;

	seq_buf_init(&s, buf, ISP_DEBUG_LINE);
	mutex_lock(&isp->video_lock);
	isp_debug_state(isp, &s, "\n");
	mutex_unlock(&isp->video_lock);
	seq_printf(m, "%s\n", seq_buf_str(&s));

	return 0;
}
DEFINE_SHOW_ATTRIBUTE(isp_debug_power);

/* The warm restart is diagnostic; a failed boot is terminal until reboot. */
static int isp_debug_firmware_cmd(struct apple_isp *isp, const char *cmd)
{
	int err;

	if (strcmp(cmd, "restart warm"))
		return -EINVAL;
	if (isp->fw_state != ISP_FW_RUNNING)
		return -EIO;

	err = pm_runtime_resume_and_get(isp->dev);
	if (err < 0)
		return err;

	apple_isp_pm_trace(isp, "debug: restart warm");
	isp->pds_keep = INT_MAX;
	apple_isp_firmware_halt(isp);
	isp->pds_keep = 0;
	isp->fw_state = ISP_FW_OFF;
	err = apple_isp_detect_camera(isp);
	/* A failed boot has its own PM reference and keeps all local domains. */
	pm_runtime_put_sync(isp->dev);
	apple_isp_pm_trace(isp, "debug: restart warm %s (%d)",
			   err ? "failed" : "done", err);

	return err;
}

static ssize_t isp_debug_firmware_write(struct file *file,
					const char __user *ubuf, size_t len,
					loff_t *off)
{
	struct apple_isp *isp = file->private_data;
	char buf[32];
	int err;

	if (!READ_ONCE(apple_isp_pm_debug))
		return -EPERM;

	if (len >= sizeof(buf))
		return -EINVAL;
	if (copy_from_user(buf, ubuf, len))
		return -EFAULT;
	buf[len] = '\0';

	mutex_lock(&isp->video_lock);
	if (test_bit(ISP_STATE_STREAMING, &isp->state) ||
	    vb2_is_busy(&isp->vbq))
		err = -EBUSY;
	else
		err = isp_debug_firmware_cmd(isp, strim(buf));
	mutex_unlock(&isp->video_lock);

	return err ?: len;
}

static const struct file_operations isp_debug_firmware_fops = {
	.owner = THIS_MODULE,
	.open = simple_open,
	.write = isp_debug_firmware_write,
	.llseek = noop_llseek,
};

/* Find the PMGR word of each power domain, to read it directly. */
static void isp_debug_init_domains(struct apple_isp *isp)
{
	struct device *dev = isp->dev;

	if (isp->pd_count <= 0)
		return;

	isp->dbg.pd_regmap = devm_kcalloc(dev, isp->pd_count,
					  sizeof(*isp->dbg.pd_regmap),
					  GFP_KERNEL);
	isp->dbg.pd_offset = devm_kcalloc(dev, isp->pd_count,
					  sizeof(*isp->dbg.pd_offset),
					  GFP_KERNEL);
	isp->dbg.pd_name = devm_kcalloc(dev, isp->pd_count,
					sizeof(*isp->dbg.pd_name), GFP_KERNEL);
	if (!isp->dbg.pd_regmap || !isp->dbg.pd_offset || !isp->dbg.pd_name) {
		isp->dbg.pd_regmap = NULL;
		return;
	}

	for (int i = 0; i < isp->pd_count; i++) {
		struct device_node *np, *parent;
		struct regmap *map;

		np = of_parse_phandle(dev->of_node, "power-domains", i);
		if (!np)
			continue;

		if (of_property_read_string(np, "label", &isp->dbg.pd_name[i]))
			isp->dbg.pd_name[i] = "?";

		parent = of_get_parent(np);
		map = parent ? syscon_node_to_regmap(parent) : ERR_PTR(-ENODEV);
		of_node_put(parent);

		if (!IS_ERR(map) &&
		    !of_property_read_u32(np, "reg", &isp->dbg.pd_offset[i]))
			isp->dbg.pd_regmap[i] = map;
		of_node_put(np);
	}
}

static void isp_debug_init_darts(struct apple_isp *isp)
{
	struct device_node *np = isp->dev->of_node;
	int count = of_count_phandle_with_args(np, "iommus", "#iommu-cells");

	for (int i = 0; i < count && isp->dbg.num_darts < ISP_MAX_DARTS; i++) {
		struct of_phandle_args args;
		struct platform_device *pdev;

		if (of_parse_phandle_with_args(np, "iommus", "#iommu-cells", i,
					       &args))
			continue;

		pdev = of_find_device_by_node(args.np);
		of_node_put(args.np);
		if (pdev)
			isp->dbg.darts[isp->dbg.num_darts++] = &pdev->dev;
	}
}

/* Find what the traces report; before anything is traced */
void apple_isp_debug_init(struct apple_isp *isp)
{
	isp_debug_init_domains(isp);
	isp_debug_init_darts(isp);
}

/* Once the driver is set up */
void apple_isp_debug_register(struct apple_isp *isp)
{
	isp->dbg.dir = debugfs_create_dir(dev_name(isp->dev), NULL);
	debugfs_create_file("power", 0400, isp->dbg.dir, isp,
			    &isp_debug_power_fops);
	debugfs_create_file("firmware", 0200, isp->dbg.dir, isp,
			    &isp_debug_firmware_fops);
}

/* With the ISP held on by the caller */
void apple_isp_debug_unregister(struct apple_isp *isp)
{
	debugfs_remove_recursive(isp->dbg.dir);
	isp->dbg.dir = NULL;

}

void apple_isp_debug_exit(struct apple_isp *isp)
{
	for (int i = 0; i < isp->dbg.num_darts; i++)
		put_device(isp->dbg.darts[i]);
	isp->dbg.num_darts = 0;
}
