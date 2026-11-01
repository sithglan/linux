// SPDX-License-Identifier: GPL-2.0-only
/* Legacy GPU batch page; doorbell redispatch preserves the guest stack. */

#include <linux/device.h>
#include <linux/io.h>
#include <linux/irqflags.h>
#include <linux/kref.h>
#include <linux/mutex.h>
#include <linux/module.h>
#include <linux/sizes.h>
#include <linux/platform_device.h>
#include <linux/slab.h>
#include <linux/soc/apple/apif-gpu.h>
#include <linux/string.h>

struct aurora_apif_gpu_batch {
	u32 count;
	u32 flags;
	s32 status;
	u32 done;
	struct aurora_apif_gpu_op op[];
};

#define AURORA_APIF_GPU_BATCH_BYTES	SZ_16K
#define AURORA_APIF_GPU_BATCH_MAX	((AURORA_APIF_GPU_BATCH_BYTES - \
				  sizeof(struct aurora_apif_gpu_batch)) / \
				 sizeof(struct aurora_apif_gpu_op))

struct aurora_apif_gpu {
	struct kref refs;
	struct mutex lock;
	bool live;
	void __iomem *doorbell;
	struct aurora_apif_gpu_batch *batch;	/* linear map, guest-PA stable */
};

static_assert(sizeof(struct aurora_apif_gpu_batch) == 16);
static_assert(sizeof(struct aurora_apif_gpu_op) == 64);

static void aurora_apif_gpu_ring(struct aurora_apif_gpu *x)
{
	unsigned long flags;
	u64 pa = virt_to_phys(x->batch);

	wmb();
	local_irq_save(flags);
	asm volatile("mov x9, sp\n"
		     "str %x[pa], [%[db]]\n"
		     "mov sp, x9\n"
		     : : [pa] "r" (pa), [db] "r" (x->doorbell) : "x9", "memory");
	local_irq_restore(flags);
	rmb();
}

/*
 * Run @count operations in order, in batches. Each op's ret is written back.
 * Stops at the first failing op and returns -EIO; *done (if given) is the
 * number of ops that completed.
 */
int aurora_apif_gpu_submit(struct aurora_apif_gpu *x, struct aurora_apif_gpu_op *ops,
		      u32 count, u32 *done)
{
	u32 total = 0;
	int ret = 0;

	mutex_lock(&x->lock);
	if (!x->live) {
		ret = -ENODEV;
		goto out;
	}
	while (total < count) {
		u32 n = min_t(u32, count - total, AURORA_APIF_GPU_BATCH_MAX);
		u32 ok, reported;

		memset(x->batch, 0, sizeof(*x->batch));
		x->batch->count = n;
		memcpy(x->batch->op, ops + total, n * sizeof(*ops));
		aurora_apif_gpu_ring(x);
		reported = READ_ONCE(x->batch->done);
		ok = min_t(u32, reported, n);
		memcpy(ops + total, x->batch->op, n * sizeof(*ops));
		total += ok;
		if (READ_ONCE(x->batch->status) || reported != n) {
			ret = -EIO;
			break;
		}
	}
out:
	mutex_unlock(&x->lock);
	if (done)
		*done = total;
	return ret;
}
EXPORT_SYMBOL_GPL(aurora_apif_gpu_submit);

static void aurora_apif_gpu_free(struct kref *refs)
{
	struct aurora_apif_gpu *x = container_of(refs, struct aurora_apif_gpu, refs);

	mutex_destroy(&x->lock);
	kfree(x);
}

void aurora_apif_gpu_put(struct aurora_apif_gpu *x)
{
	kref_put(&x->refs, aurora_apif_gpu_free);
}
EXPORT_SYMBOL_GPL(aurora_apif_gpu_put);

/* Registered after the batch and MMIO resources, so it runs before either
 * is released. Drain a synchronous submission, then reject all later ones.
 * Client references can survive unbind without retaining access to MMIO.
 */
static void aurora_apif_gpu_revoke(void *data)
{
	struct aurora_apif_gpu *x = data;

	mutex_lock(&x->lock);
	x->live = false;
	x->doorbell = NULL;
	x->batch = NULL;
	mutex_unlock(&x->lock);
	aurora_apif_gpu_put(x);
}

struct aurora_apif_gpu *aurora_apif_gpu_get(struct device *dev, const char *reg_name,
			       u32 doorbell)
{
	struct platform_device *pdev = to_platform_device(dev);
	struct aurora_apif_gpu *x;
	struct resource *resource;
	void __iomem *alias;
	int ret;

	if (doorbell & 7 || doorbell >= SZ_16K)
		return ERR_PTR(-EINVAL);
	resource = platform_get_resource_byname(pdev, IORESOURCE_MEM, reg_name);
	if (!resource || resource_size(resource) < (u64)doorbell + sizeof(u64))
		return ERR_PTR(-EINVAL);
	x = kzalloc(sizeof(*x), GFP_KERNEL);
	if (!x)
		return ERR_PTR(-ENOMEM);
	kref_init(&x->refs);
	mutex_init(&x->lock);
	alias = devm_platform_ioremap_resource_byname(pdev, reg_name);
	if (IS_ERR(alias)) {
		ret = PTR_ERR(alias);
		goto put;
	}
	/* kmalloc of 16 KiB is physically contiguous and in the linear map. */
	x->batch = devm_kzalloc(dev, AURORA_APIF_GPU_BATCH_BYTES, GFP_KERNEL);
	if (!x->batch) {
		ret = -ENOMEM;
		goto put;
	}
	x->doorbell = alias + doorbell;
	x->live = true;
	/* One reference for the returned client and one for devres revocation. */
	kref_get(&x->refs);
	ret = devm_add_action_or_reset(dev, aurora_apif_gpu_revoke, x);
	if (ret)
		goto put;
	return x;
put:
	aurora_apif_gpu_put(x);
	return ERR_PTR(ret);
}
EXPORT_SYMBOL_GPL(aurora_apif_gpu_get);

MODULE_DESCRIPTION("Aurora APIF GPU batch transport");
MODULE_LICENSE("GPL");
