/* SPDX-License-Identifier: GPL-2.0-only OR MIT */
#ifndef _LINUX_SOC_APPLE_PMP_TELEMETRY_H
#define _LINUX_SOC_APPLE_PMP_TELEMETRY_H

struct device;

/*
 * Called by the PMP owner: register once its firmware runs, stop when the
 * firmware crashes or is stopped. The telemetry is device-managed on @dev.
 */
int apple_pmp_telemetry_register(struct device *dev);
void apple_pmp_telemetry_stop(struct device *dev);

#endif
