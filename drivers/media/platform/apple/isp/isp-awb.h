/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef __ISP_AWB_H__
#define __ISP_AWB_H__

struct apple_isp;
struct isp_buffer;

void apple_isp_awb_init(struct apple_isp *isp);
int apple_isp_awb_start(struct apple_isp *isp, u32 ch);
void apple_isp_awb_stop(struct apple_isp *isp);
void apple_isp_awb_frame(struct apple_isp *isp, struct isp_buffer *buf);

#endif
