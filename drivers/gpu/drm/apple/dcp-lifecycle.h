/* SPDX-License-Identifier: GPL-2.0-only OR MIT */
/* Copyright The Asahi Linux Contributors */

#ifndef __APPLE_DCP_LIFECYCLE_H__
#define __APPLE_DCP_LIFECYCLE_H__

#include <linux/bits.h>
#include <linux/types.h>

#define APPLE_DCP_COPROC_CPU_CONTROL_RUN	BIT(4)
#define APPLE_DCP_COPROC_CPU_STATUS_RUNNING	BIT(0)
#define APPLE_DCP_COPROC_CPU_STATUS_STOPPED	BIT(1)

enum apple_dcp_coprocessor_start {
	APPLE_DCP_COPROCESSOR_COLD_START,
	APPLE_DCP_COPROCESSOR_ADOPT_AND_RESTART,
	APPLE_DCP_COPROCESSOR_QUIESCED_RESTART,
};

static inline bool apple_dcp_coprocessor_is_active(u32 cpu_control, u32 cpu_status)
{
	if (cpu_control & APPLE_DCP_COPROC_CPU_CONTROL_RUN ||
	    cpu_status & APPLE_DCP_COPROC_CPU_STATUS_RUNNING)
		return true;

	/* A zero status is not evidence that the coprocessor stopped. */
	return !(cpu_status & APPLE_DCP_COPROC_CPU_STATUS_STOPPED);
}

static inline enum apple_dcp_coprocessor_start
apple_dcp_coprocessor_start_mode(bool loader_quiesced, bool can_adopt,
				 u32 cpu_control, u32 cpu_status)
{
	if (loader_quiesced)
		return APPLE_DCP_COPROCESSOR_QUIESCED_RESTART;

	if (can_adopt &&
	    apple_dcp_coprocessor_is_active(cpu_control, cpu_status))
		return APPLE_DCP_COPROCESSOR_ADOPT_AND_RESTART;

	return APPLE_DCP_COPROCESSOR_COLD_START;
}

#endif /* __APPLE_DCP_LIFECYCLE_H__ */
