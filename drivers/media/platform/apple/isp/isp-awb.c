// SPDX-License-Identifier: GPL-2.0-only
/*
 * Host white balance for ISPs whose own AWB runs on the neural engine.
 *
 * The T8142 (M5) ISP firmware's AWB is the semantic one, which segments the
 * picture on the ANE. Without that link the firmware keeps the setfile's
 * default gains (a magenta cast on the J813 IMX958). The firmware also has a
 * manual scheme: AWB scheme 4 with a manual colour temperature takes R/G/B
 * gains from the host (CISP_CMD_CH_AWB_MANUAL_WB_GAIN_SET), applied before
 * the colour matrix. This file closes that loop: it samples every few
 * rendered NV12 frames on a sparse grid, takes the mean R, G and B of the
 * near-neutral pixels (grey world without the coloured ones) and moves the
 * gains a step towards making them equal.
 */
#include <linux/highmem.h>
#include <linux/module.h>
#include <linux/scatterlist.h>
#include <media/videobuf2-dma-sg.h>

#include "isp-awb.h"
#include "isp-cmd.h"
#include "isp-drv.h"

#define ISP_AWB_SCHEME_MANUAL 4
#define ISP_AWB_CCT 5000
#define ISP_AWB_UNITY 0x1000
#define ISP_AWB_MAX 0x4000
#define ISP_AWB_PERIOD 4	/* frames between updates */
#define ISP_AWB_GRID_X 40
#define ISP_AWB_GRID_Y 24

static bool host_awb = true;
module_param(host_awb, bool, 0644);
MODULE_PARM_DESC(host_awb, "Host grey-world white balance where the ISP's AWB needs the ANE");

static bool awb_debug;
module_param(awb_debug, bool, 0644);
MODULE_PARM_DESC(awb_debug, "Log the host white balance statistics and gains");

/* Reads one byte at @off of a page-backed buffer, the iterator kept ascending. */
struct isp_awb_reader {
	struct sg_page_iter it;
	size_t base;	/* buffer offset of the current page */
	bool valid;
};

static int isp_awb_byte(struct sg_table *sgt, struct isp_awb_reader *r, size_t off)
{
	u8 *p, v;

	if (!r->valid) {
		__sg_page_iter_start(&r->it, sgt->sgl, sgt->orig_nents, 0);
		if (!__sg_page_iter_next(&r->it))
			return -1;
		r->base = 0;
		r->valid = true;
	}
	while (off >= r->base + PAGE_SIZE) {
		if (!__sg_page_iter_next(&r->it))
			return -1;
		r->base += PAGE_SIZE;
	}
	if (off < r->base)
		return -1;
	p = kmap_local_page(sg_page_iter_page(&r->it));
	v = p[off - r->base];
	kunmap_local(p);
	return v;
}

void apple_isp_awb_frame(struct apple_isp *isp, struct isp_buffer *buf)
{
	const struct isp_format *fmt = isp_get_current_format(isp);
	struct isp_awb *awb = &isp->awb;
	struct isp_awb_reader ry = {}, ruv = {};
	u64 sum[3] = {};
	struct sg_table *sgt;
	unsigned int i, j, w, h, n = 0;
	size_t stride;

	if (!READ_ONCE(awb->active))
		return;
	/* The firmware resets its AWB setup when the channel starts: set it up once frames flow. */
	if (READ_ONCE(awb->setup)) {
		if (++awb->frames == 2)
			queue_work(isp->wq, &awb->work);
		return;
	}
	if (++awb->frames % ISP_AWB_PERIOD)
		return;
	if (fmt->pixelformat != V4L2_PIX_FMT_NV12 || !fmt->strides[0] || !fmt->preset)
		return;
	sgt = vb2_dma_sg_plane_desc(&buf->vb.vb2_buf, 0);
	if (!sgt)
		return;
	stride = fmt->strides[0];
	/* NV12: the CbCr plane follows the luma plane with the same stride. */
	w = fmt->preset->output_dim.x;
	h = fmt->plane_size[0] / stride;
	if (!w || !h)
		return;

	for (j = 0; j < ISP_AWB_GRID_Y; j++) {
		unsigned int y = (2 * j + 1) * h / (2 * ISP_AWB_GRID_Y) & ~1u;

		for (i = 0; i < ISP_AWB_GRID_X; i++) {
			unsigned int x = (2 * i + 1) * w / (2 * ISP_AWB_GRID_X) & ~1u;
			int Y, U, V, c, r, g, b;

			Y = isp_awb_byte(sgt, &ry, y * stride + x);
			U = isp_awb_byte(sgt, &ruv, fmt->plane_size[0] + (y / 2) * stride + x);
			V = isp_awb_byte(sgt, &ruv, fmt->plane_size[0] + (y / 2) * stride + x + 1);
			if (Y < 0 || U < 0 || V < 0) {
				if (awb_debug)
					dev_info_ratelimited(isp->dev, "awb: read failed at %u,%u\n",
							     x, y);
				return;
			}
			/* Clipped, black or strongly coloured pixels say nothing about the light. */
			if (Y < 40 || Y > 225 || abs(U - 128) + abs(V - 128) > 48)
				continue;
			c = 298 * (Y - 16);
			r = (c + 409 * (V - 128)) >> 8;
			g = (c - 100 * (U - 128) - 208 * (V - 128)) >> 8;
			b = (c + 516 * (U - 128)) >> 8;
			if (r <= 0 || g <= 0 || b <= 0)
				continue;
			sum[0] += r;
			sum[1] += g;
			sum[2] += b;
			n++;
		}
	}
	if (awb_debug)
		dev_info_ratelimited(isp->dev, "awb: %u samples, sums %llu %llu %llu\n", n,
				     sum[0], sum[1], sum[2]);
	if (n < ISP_AWB_GRID_X * ISP_AWB_GRID_Y / 8)
		return;
	spin_lock(&awb->lock);
	memcpy(awb->sum, sum, sizeof(sum));
	awb->n = n;
	spin_unlock(&awb->lock);
	queue_work(isp->wq, &awb->work);
}

static void isp_awb_work(struct work_struct *work)
{
	struct isp_awb *awb = container_of(work, struct isp_awb, work);
	struct apple_isp *isp = container_of(awb, struct apple_isp, awb);
	u64 sum[3], gain[3], min;
	unsigned int i;
	u32 n;

	if (!READ_ONCE(awb->active))
		return;
	if (READ_ONCE(awb->setup)) {
		int err = isp_cmd_ch_awb_scheme_set(isp, awb->chan, ISP_AWB_SCHEME_MANUAL);

		if (!err)
			err = isp_cmd_ch_awb_cct_manual(isp, awb->chan, ISP_AWB_CCT);
		if (!err)
			err = isp_cmd_ch_awb_manual_wb_gain_set(isp, awb->chan, awb->gain[0],
								awb->gain[1], awb->gain[2]);
		if (err)
			dev_warn(isp->dev, "host white balance not set up: %d\n", err);
		awb->frames = 0;
		WRITE_ONCE(awb->setup, false);
		return;
	}
	spin_lock(&awb->lock);
	memcpy(sum, awb->sum, sizeof(sum));
	n = awb->n;
	spin_unlock(&awb->lock);
	if (!n || !sum[0] || !sum[1] || !sum[2])
		return;

	/*
	 * Half a step towards R = G = B: gain *= sqrt(mean_g / mean_x), done
	 * as the average of the old gain and the full correction.
	 */
	for (i = 0; i < 3; i++) {
		u64 full = div64_u64((u64)awb->gain[i] * sum[1], sum[i]);

		gain[i] = (awb->gain[i] + full) / 2;
	}
	/* The firmware takes no gain below unity: scale the smallest to 1.0. */
	min = min3(gain[0], gain[1], gain[2]);
	for (i = 0; i < 3; i++)
		gain[i] = clamp_t(u64, div64_u64(gain[i] * ISP_AWB_UNITY, min), ISP_AWB_UNITY,
				  ISP_AWB_MAX);
	if (awb_debug)
		dev_info(isp->dev, "awb: gains %#x %#x %#x -> %#llx %#llx %#llx\n", awb->gain[0],
			 awb->gain[1], awb->gain[2], gain[0], gain[1], gain[2]);
	/* Hold still inside a small dead band. */
	for (i = 0; i < 3; i++)
		if (abs((s64)gain[i] - awb->gain[i]) > 0x20)
			break;
	if (i == 3)
		return;
	/*
	 * Outside the video lock: the command area has its own lock, and
	 * stopping cancels this work before the stop commands go out.
	 */
	if (!isp_cmd_ch_awb_manual_wb_gain_set(isp, awb->chan, gain[0], gain[1], gain[2]))
		for (i = 0; i < 3; i++)
			awb->gain[i] = gain[i];
}

void apple_isp_awb_init(struct apple_isp *isp)
{
	INIT_WORK(&isp->awb.work, isp_awb_work);
	spin_lock_init(&isp->awb.lock);
}

/*
 * Per stream: switch the firmware to manual gains once the first frames are
 * out (scheme 4, a manual colour temperature, then the gains).
 */
int apple_isp_awb_start(struct apple_isp *isp, u32 ch)
{
	struct isp_awb *awb = &isp->awb;

	if (!host_awb || !isp->hw->host_awb)
		return 0;
	/* Start near neutral for indoor light; the loop takes it from there. */
	awb->gain[0] = ISP_AWB_UNITY;
	awb->gain[1] = 0x1100;
	awb->gain[2] = 0x1080;
	awb->chan = ch;
	awb->frames = 0;
	WRITE_ONCE(awb->setup, true);
	WRITE_ONCE(awb->active, true);
	return 0;
}

void apple_isp_awb_stop(struct apple_isp *isp)
{
	WRITE_ONCE(isp->awb.active, false);
	cancel_work_sync(&isp->awb.work);
}
