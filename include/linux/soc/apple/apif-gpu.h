/* SPDX-License-Identifier: GPL-2.0-only */
/* Compatibility GPU operation page. This is distinct from native APIF v2. */
#ifndef _LINUX_SOC_AURORA_APIF_GPU_H_
#define _LINUX_SOC_AURORA_APIF_GPU_H_

#include <linux/types.h>

struct device;

/* Doorbell offsets inside an alias page. */
#define AURORA_APIF_GPU_UAT_DOORBELL		0x1008

/* UAT (GPU) operations; see x1n1 src/sptm.c SPTM_HV_UAT_*. */
#define AURORA_APIF_GPU_UAT_CREATE		1 /* ()                          -> handle */
#define AURORA_APIF_GPU_UAT_MAP_TABLE	2 /* (handle, va, level) */
#define AURORA_APIF_GPU_UAT_MAP		3 /* (handle, va, pa, npages, opts) */
#define AURORA_APIF_GPU_UAT_UNMAP		4 /* (handle, va, npages) */
#define AURORA_APIF_GPU_UAT_SET_CTX		5 /* (handle, ctx) */
#define AURORA_APIF_GPU_UAT_REMOVE_CTX	6 /* (handle) */
#define AURORA_APIF_GPU_UAT_DESTROY		7 /* (handle) */
/* SPTM's global firmware/kernel object: MAP_TABLE (L3 in the kernel window),
 * MAP and UNMAP only. */
#define AURORA_APIF_GPU_UAT_GLOBAL		0xff
/* One MAP/UNMAP covers at most this many 16 KiB pages (SPTM GET_INFO 5). */
#define AURORA_APIF_GPU_UAT_MAX_PAGES	0x40

struct aurora_apif_gpu_op {
	u32 op;
	u32 flags;
	u64 arg[6];
	u64 ret;
};

struct aurora_apif_gpu;

/* Returns a client reference; release it with aurora_apif_gpu_put(). Device
 * unbind drains in-flight calls and makes subsequent submissions fail ENODEV.
 */
struct aurora_apif_gpu *aurora_apif_gpu_get(struct device *dev, const char *reg_name,
				       u32 doorbell);
void aurora_apif_gpu_put(struct aurora_apif_gpu *x);
int aurora_apif_gpu_submit(struct aurora_apif_gpu *x, struct aurora_apif_gpu_op *ops,
		      u32 count, u32 *done);

#endif
