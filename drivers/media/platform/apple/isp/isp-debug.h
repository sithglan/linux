/* SPDX-License-Identifier: GPL-2.0-only */
/* Copyright The Asahi Linux Contributors */

#ifndef __ISP_DEBUG_H__
#define __ISP_DEBUG_H__

#include "isp-drv.h"

extern unsigned int apple_isp_pm_debug;

void apple_isp_debug_init(struct apple_isp *isp);
void apple_isp_debug_register(struct apple_isp *isp);
void apple_isp_debug_unregister(struct apple_isp *isp);
void apple_isp_debug_exit(struct apple_isp *isp);
__printf(3, 4)
void __apple_isp_pm_trace(struct apple_isp *isp, bool step, const char *fmt,
			  ...);

/*
 * Power-path tracing, enabled with the pm_debug parameter. A trace logs
 * the power state of the ISP; a step does the same before an action that
 * may hang the machine, and then waits pm_debug_delay_ms so that the line
 * can reach storage first.
 */
#define apple_isp_pm_trace(isp, fmt, ...)                                  \
	do {                                                               \
		if (READ_ONCE(apple_isp_pm_debug))                         \
			__apple_isp_pm_trace(isp, false, fmt, ##__VA_ARGS__); \
	} while (0)

#define apple_isp_pm_step(isp, fmt, ...)                                   \
	do {                                                               \
		if (READ_ONCE(apple_isp_pm_debug))                         \
			__apple_isp_pm_trace(isp, true, fmt, ##__VA_ARGS__); \
	} while (0)

#endif /* __ISP_DEBUG_H__ */
