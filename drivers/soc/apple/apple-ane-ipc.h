/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef _APPLE_ANE_IPC_H
#define _APPLE_ANE_IPC_H

#include <linux/compiler.h>
#include <linux/dma-mapping.h>
#include <linux/errno.h>
#include <linux/types.h>
#include <linux/unaligned.h>

/* Observed J700 H17 / 25G83 transport. Doorbells are the caller's job.
 * Synchronous IO replies return ownership of the submitted IO slot.
 * IO_T2H carries separate asynchronous messages, including inference events.
 * Bit 1 is not interpreted here; this synchronous path requires it clear.
 */
struct apple_ane_ipc_slot {
	__le64 pointer;
	__le64 length;
	__le64 metadata;
	u8 reserved[40];
};

struct apple_ane_io_transaction {
	struct apple_ane_ipc_slot *slot;
	u64 iova;
	u32 capacity;
	bool pending;
};

static inline int apple_ane_io_publish(struct apple_ane_io_transaction *tx,
				     struct apple_ane_ipc_slot *slot,
				     u64 iova, u32 length)
{
	u64 pointer;

	if (!tx || !slot || !iova || (iova & 3) || length < 8 ||
	    length > 0x40000 || iova >= (1ULL << 42) ||
	    length > (1ULL << 42) - iova)
		return -EINVAL;
	if (tx->pending)
		return -EBUSY;
	pointer = le64_to_cpu(READ_ONCE(slot->pointer));
	if (!(pointer & 1))
		return -EBUSY;
	if (pointer & 2)
		return -EPROTO;

	/* The caller filled a DMA-coherent command buffer before publication. */
	WRITE_ONCE(slot->length, cpu_to_le64(length));
	WRITE_ONCE(slot->metadata, cpu_to_le64(length));
	dma_wmb();
	WRITE_ONCE(slot->pointer, cpu_to_le64(iova));
	dma_wmb();
	tx->slot = slot;
	tx->iova = iova;
	tx->capacity = length;
	tx->pending = true;
	return 0;
}

static inline int apple_ane_io_poll(struct apple_ane_io_transaction *tx,
				  u32 *length, u64 *metadata)
{
	u64 pointer, reply_length;

	if (!tx || !tx->pending || !tx->slot || !length || !metadata)
		return -EINVAL;
	pointer = le64_to_cpu(READ_ONCE(tx->slot->pointer));
	if ((pointer & ~1ULL) != tx->iova)
		return -EPROTO;
	if (!(pointer & 1))
		return -EAGAIN;
	/* Ownership is returned only after firmware published its reply. */
	dma_rmb();
	reply_length = le64_to_cpu(READ_ONCE(tx->slot->length));
	if (reply_length < 8 || reply_length > tx->capacity)
		return -EPROTO;
	*length = reply_length;
	*metadata = le64_to_cpu(READ_ONCE(tx->slot->metadata));
	tx->pending = false;
	return 0;
}

/* Only the allocation-request form is established by the current trace.
 * Do not mistake it for a payload pointer: pointer=0, length=requested bytes,
 * metadata=tag. A successful reply is IOVA|1, length=0, metadata=opaque handle.
 * The caller must retain the mapped allocation until firmware releases it or
 * a qualified shutdown proves DMA stopped; this helper never frees memory.
 */
static inline int apple_ane_alloc_request(struct apple_ane_ipc_slot *slot,
					 u32 limit, u32 *size, u64 *tag)
{
	u64 pointer, bytes;

	if (!slot || !limit || !size || !tag)
		return -EINVAL;
	pointer = le64_to_cpu(READ_ONCE(slot->pointer));
	if (pointer & 1)
		return -EAGAIN;
	if (pointer)
		return -EOPNOTSUPP;
	dma_rmb();
	bytes = le64_to_cpu(READ_ONCE(slot->length));
	if (!bytes || bytes > limit)
		return -EOVERFLOW;
	*size = bytes;
	*tag = le64_to_cpu(READ_ONCE(slot->metadata));
	return 0;
}

static inline int apple_ane_alloc_reply(struct apple_ane_ipc_slot *slot,
				       u32 expected_size, u64 expected_tag,
				       u64 iova, u64 handle)
{
	if (!slot || !expected_size || !handle || !iova || (iova & 0x3fff) ||
	    iova >= (1ULL << 42) || expected_size > (1ULL << 42) - iova)
		return -EINVAL;
	if (le64_to_cpu(READ_ONCE(slot->pointer)))
		return -EBUSY;
	dma_rmb();
	if (le64_to_cpu(READ_ONCE(slot->length)) != expected_size ||
	    le64_to_cpu(READ_ONCE(slot->metadata)) != expected_tag)
		return -EPROTO;
	WRITE_ONCE(slot->length, cpu_to_le64(0));
	WRITE_ONCE(slot->metadata, cpu_to_le64(handle));
	dma_wmb();
	WRITE_ONCE(slot->pointer, cpu_to_le64(iova | 1));
	dma_wmb();
	return 0;
}

/* Qualified 25G83 inference event. Word +0 is a reusable opaque handle:
 * jobs one/two use 0xfe00..0xfe03, then job three reuses 0xfe00. It is not
 * a kind or global sequence. Stage at +0x1c is begin/end, and token matches
 * qword +0x20 of the submitted request. Accept only the observed handle set.
 * Caller validates phase ordering and copies the payload before release.
 */
static inline int apple_ane_work_event(const void *payload, u32 size,
				     u64 token, bool *completed)
{
	const u8 *data = payload;
	u32 identifier, stage;

	if (!payload || !completed || size != 96)
		return -EINVAL;
	identifier = get_unaligned_le32(data);
	stage = get_unaligned_le32(data + 0x1c);
	if (identifier < 0xfe00 || identifier > 0xfe03 ||
	    get_unaligned_le16(data + 4) != 0x300)
		return -EOPNOTSUPP;
	if (get_unaligned_le64(data + 8) != token)
		return -ESTALE;
	if (get_unaligned_le16(data + 6))
		return -EIO;
	if (stage > 1 || get_unaligned_le64(data + 0x10) ||
	    get_unaligned_le32(data + 0x18) || get_unaligned_le64(data + 0x20))
		return -EPROTO;
	*completed = stage == 1;
	return 0;
}

/* The tiny Llama program emits one additional stage-3 event between begin
 * and end. Three native jobs used handles 0, 0x10001, 0x20002. This helper is
 * deliberately limited to those three jobs; it does not signal completion.
 */
static inline int apple_ane_profile_event(const void *payload, u32 size,
					u64 token, u32 sample)
{
	const u8 *data = payload;

	if (!payload || size != 96 || sample >= 3)
		return -EINVAL;
	if (get_unaligned_le64(data + 8) != token)
		return -ESTALE;
	if (get_unaligned_le16(data + 6))
		return -EIO;
	if (get_unaligned_le32(data) != sample * 0x10001U ||
	    get_unaligned_le16(data + 4) != 0x300 ||
	    get_unaligned_le32(data + 0x1c) != 3 ||
	    get_unaligned_le64(data + 0x10) || get_unaligned_le32(data + 0x18) ||
	    get_unaligned_le64(data + 0x20) || get_unaligned_le64(data + 0x30) != 0xff00)
		return -EPROTO;
	return 0;
}

/* Runtime transport validates the message semantics rather than assigning
 * meaning to the opaque handle at word zero. Ownership is returned through
 * the original mapped pointer. Token, opcode, status and phase still match
 * the sole in-flight job; the caller enforces begin/profile/end ordering.
 */
static inline int apple_ane_session_event(const void *payload, u32 size,
					u64 token, u32 *stage)
{
	const u8 *data = payload;
	u32 phase;

	if (!payload || !stage || size != 96 || !token)
		return -EINVAL;
	if (get_unaligned_le64(data + 8) != token)
		return -ESTALE;
	if (get_unaligned_le16(data + 4) != 0x300)
		return -EOPNOTSUPP;
	if (get_unaligned_le16(data + 6))
		return -EIO;
	phase = get_unaligned_le32(data + 0x1c);
	if ((phase != 0 && phase != 1 && phase != 3) ||
	    get_unaligned_le64(data + 0x10) || get_unaligned_le32(data + 0x18) ||
	    get_unaligned_le64(data + 0x20) ||
	    get_unaligned_le64(data + 0x30) != (phase == 3 ? 0xff00 : 0))
		return -EPROTO;
	*stage = phase;
	return 0;
}

#endif
