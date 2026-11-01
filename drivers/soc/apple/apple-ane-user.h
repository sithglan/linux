/* SPDX-License-Identifier: GPL-2.0 WITH Linux-syscall-note */
#ifndef _AURORA_ANE_USER_H
#define _AURORA_ANE_USER_H

#include <linux/ioctl.h>
#include <linux/types.h>

/* Private experimental interface. Not an upstream/stable Linux UAPI.
 * Only the locally SHA-pinned stories260K program is accepted. Userspace
 * supplies tensor bytes, never firmware commands, IOVAs or physical memory.
 */
#define AURORA_ANE_ABI 1U
#define AURORA_ANE_MODEL_TINY260K 1U
#define AURORA_ANE_MAX_JOBS 256U
#define AURORA_ANE_RESET_REQUIRED 1U
#define AURORA_ANE_STATE_READY 0U
#define AURORA_ANE_STATE_LOADED 1U
#define AURORA_ANE_STATE_CLOSED 2U
#define AURORA_ANE_STATE_FAULT 3U

struct aurora_ane_info {
	__u32 abi, model, input_bytes, output_bytes;
	__u32 max_jobs, completed_jobs, state, flags;
	__s32 last_error;
	__u32 reserved[3];
	__aligned_u64 events;
	__u8 case_sha256[32];
};

struct aurora_ane_run {
	__u32 abi, flags, input_bytes, output_bytes;
	__aligned_u64 input, output;
	__aligned_u64 cookie;
	/* Output fields: zero on submission. A completed job remains counted if
	 * copying output back faults. Query INFO before deciding to resubmit.
	 */
	__aligned_u64 sequence, elapsed_ns;
	__u32 reserved[2];
};

#define AURORA_ANE_INFO _IOR('J', 0xa0, struct aurora_ane_info)
#define AURORA_ANE_LOAD _IO('J', 0xa1)
#define AURORA_ANE_RUN _IOWR('J', 0xa2, struct aurora_ane_run)
#define AURORA_ANE_SHUTDOWN _IO('J', 0xa3)

#endif
