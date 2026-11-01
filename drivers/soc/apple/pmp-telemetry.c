// SPDX-License-Identifier: GPL-2.0-only OR MIT
/*
 * Apple PMP temperature telemetry: the sensor records the running PMP
 * firmware publishes in its SRAM, as an "apple_pmp" hwmon device. temp1 is
 * the hottest valid record, the others are the records themselves.
 *
 * After the J713 telemetry in erikwb/linux j713/daily (Erik Bourget, Eryk
 * Wieliczko); the CPU/GPU thermal policy built on it there is J713-specific
 * and not part of this.
 */
#include <linux/device.h>
#include <linux/hwmon.h>
#include <linux/io.h>
#include <linux/list.h>
#include <linux/module.h>
#include <linux/mutex.h>
#include <linux/of.h>
#include <linux/platform_device.h>
#include <linux/slab.h>
#include <linux/soc/apple/pmp-telemetry.h>
#include <linux/soc/apple/pmp-temps.h>

/* ASC CPU_CONTROL and its run bit: the records are live only while set. */
#define PMP_ASC_CPU_CONTROL	0x44
#define PMP_ASC_CPU_RUN		BIT(4)

struct apple_pmp_telemetry {
	struct device *dev;
	struct list_head list;
	const struct apple_pmp_temp_table *table;
	void __iomem *sram;
	void __iomem *asc;
	bool online;
	u32 *config;
	struct hwmon_channel_info temp;
	const struct hwmon_channel_info *info[2];
	struct hwmon_chip_info chip;
};

static LIST_HEAD(apple_pmp_telemetry_list);
static DEFINE_MUTEX(apple_pmp_telemetry_lock);

static bool apple_pmp_telemetry_online(struct apple_pmp_telemetry *t)
{
	return READ_ONCE(t->online) &&
	       (readl(t->asc + PMP_ASC_CPU_CONTROL) & PMP_ASC_CPU_RUN);
}

static int apple_pmp_telemetry_hotspot(struct apple_pmp_telemetry *t, long *value)
{
	bool found = false;
	long sample;
	unsigned int i;
	int ret;

	for (i = 0; i < t->table->count; i++) {
		ret = apple_pmp_temp_sample(t->sram, &t->table->records[i], &sample);
		if (ret == -ENODATA)
			continue;
		if (ret)
			return ret;
		if (!found || sample > *value)
			*value = sample;
		found = true;
	}
	return found ? 0 : -ENODATA;
}

static umode_t apple_pmp_hwmon_visible(const void *data,
				       enum hwmon_sensor_types type, u32 attr,
				       int channel)
{
	if (type == hwmon_temp &&
	    (attr == hwmon_temp_input || attr == hwmon_temp_label))
		return 0444;
	return 0;
}

static int apple_pmp_hwmon_read(struct device *dev,
				enum hwmon_sensor_types type, u32 attr,
				int channel, long *value)
{
	struct apple_pmp_telemetry *t = dev_get_drvdata(dev);
	int ret;

	if (type != hwmon_temp || attr != hwmon_temp_input)
		return -EOPNOTSUPP;
	if (channel < 0 || channel > t->table->count)
		return -EINVAL;
	if (!apple_pmp_telemetry_online(t))
		return -ENODEV;
	if (channel)
		ret = apple_pmp_temp_sample(t->sram,
					    &t->table->records[channel - 1], value);
	else
		ret = apple_pmp_telemetry_hotspot(t, value);
	/* A firmware stop concurrent with the read invalidates it. */
	return apple_pmp_telemetry_online(t) ? ret : -ENODEV;
}

static int apple_pmp_hwmon_label(struct device *dev,
				 enum hwmon_sensor_types type, u32 attr,
				 int channel, const char **label)
{
	struct apple_pmp_telemetry *t = dev_get_drvdata(dev);

	if (type != hwmon_temp || attr != hwmon_temp_label ||
	    channel < 0 || channel > t->table->count)
		return -EOPNOTSUPP;
	*label = channel ? t->table->records[channel - 1].label :
			   "PMP hotspot";
	return 0;
}

static const struct hwmon_ops apple_pmp_hwmon_ops = {
	.is_visible = apple_pmp_hwmon_visible,
	.read = apple_pmp_hwmon_read,
	.read_string = apple_pmp_hwmon_label,
};

static void apple_pmp_telemetry_release(void *data)
{
	struct apple_pmp_telemetry *t = data;

	mutex_lock(&apple_pmp_telemetry_lock);
	list_del(&t->list);
	mutex_unlock(&apple_pmp_telemetry_lock);
}

int apple_pmp_telemetry_register(struct device *dev)
{
	const struct apple_pmp_temp_table *table = NULL;
	struct platform_device *pdev = to_platform_device(dev);
	struct apple_pmp_telemetry *t;
	struct resource *sram, *asc;
	struct device *hwmon;
	unsigned int i;
	u32 end = 0;
	int ret;

	if (!IS_REACHABLE(CONFIG_HWMON))
		return 0;
	for (i = 0; i < ARRAY_SIZE(apple_pmp_temp_tables); i++)
		if (of_device_is_compatible(dev->of_node,
					    apple_pmp_temp_tables[i].compatible))
			table = &apple_pmp_temp_tables[i];
	if (!table)
		return 0;
	for (i = 0; i < table->count; i++)
		end = max(end, table->records[i].offset + PMP_TEMP_RECORD_SIZE);

	sram = platform_get_resource_byname(pdev, IORESOURCE_MEM, "pmp");
	asc = platform_get_resource_byname(pdev, IORESOURCE_MEM, "asc");
	if (!sram || !asc || resource_size(sram) < end ||
	    resource_size(asc) < PMP_ASC_CPU_CONTROL + 4)
		return -EINVAL;

	t = devm_kzalloc(dev, sizeof(*t), GFP_KERNEL);
	if (!t)
		return -ENOMEM;
	t->config = devm_kcalloc(dev, table->count + 2, sizeof(*t->config),
				 GFP_KERNEL);
	if (!t->config)
		return -ENOMEM;
	/* The PMP owner holds the regions; these are read-only second maps. */
	t->sram = devm_ioremap(dev, sram->start, end);
	t->asc = devm_ioremap(dev, asc->start, PMP_ASC_CPU_CONTROL + 4);
	if (!t->sram || !t->asc)
		return -ENOMEM;

	t->dev = dev;
	t->table = table;
	for (i = 0; i <= table->count; i++)
		t->config[i] = HWMON_T_INPUT | HWMON_T_LABEL;
	t->temp.type = hwmon_temp;
	t->temp.config = t->config;
	t->info[0] = &t->temp;
	t->chip.ops = &apple_pmp_hwmon_ops;
	t->chip.info = t->info;
	WRITE_ONCE(t->online, true);

	mutex_lock(&apple_pmp_telemetry_lock);
	list_add_tail(&t->list, &apple_pmp_telemetry_list);
	mutex_unlock(&apple_pmp_telemetry_lock);
	ret = devm_add_action_or_reset(dev, apple_pmp_telemetry_release, t);
	if (ret)
		return ret;

	hwmon = devm_hwmon_device_register_with_info(dev, "apple_pmp", t,
						     &t->chip, NULL);
	return PTR_ERR_OR_ZERO(hwmon);
}
EXPORT_SYMBOL_GPL(apple_pmp_telemetry_register);

void apple_pmp_telemetry_stop(struct device *dev)
{
	struct apple_pmp_telemetry *t;

	mutex_lock(&apple_pmp_telemetry_lock);
	list_for_each_entry(t, &apple_pmp_telemetry_list, list)
		if (t->dev == dev)
			WRITE_ONCE(t->online, false);
	mutex_unlock(&apple_pmp_telemetry_lock);
}
EXPORT_SYMBOL_GPL(apple_pmp_telemetry_stop);

MODULE_DESCRIPTION("Apple PMP temperature telemetry");
MODULE_LICENSE("Dual MIT/GPL");
