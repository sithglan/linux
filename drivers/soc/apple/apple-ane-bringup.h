/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef _APPLE_ANE_BRINGUP_H
#define _APPLE_ANE_BRINGUP_H

struct device;

/* Holds both the exact J700 sequencer device and an active runtime-PM use.
 * The firmware test must retain this hold until DMA is known to have stopped.
 */
int apple_ane_bringup_get_engine(struct device **held);
void apple_ane_bringup_put_engine(struct device *held);

#endif
