// SPDX-License-Identifier: GPL-2.0-only
/* J700/25G83 RAM-only firmware transport experiment, not a userspace ABI.
 *
 * The companion test overlay holds the qualified power sequencer before any
 * DART device is created. This driver claims a private Linux IOMMU domain for
 * SID 0 on all three DARTs. It never programs their translation registers.
 *
 * Once RUN is published this module pins itself. There is deliberately no
 * speculative stop/free path: a Neo reboot is required after this experiment.
 * Loading the driver does not start firmware; write 25G83 to start_bootstrap.
 */
#include <crypto/sha2.h>
#include <linux/delay.h>
#include <linux/capability.h>
#include <linux/fs.h>
#include <linux/ktime.h>
#include <linux/miscdevice.h>
#include <linux/uaccess.h>
#include <linux/device.h>
#include <linux/dma-direct.h>
#include <linux/firmware.h>
#include <linux/io.h>
#include <linux/iommu.h>
#include <linux/iopoll.h>
#include <linux/jiffies.h>
#include <linux/list.h>
#include <linux/mm.h>
#include <linux/module.h>
#include <linux/mutex.h>
#include <linux/of.h>
#include <linux/of_address.h>
#include <linux/of_platform.h>
#include <linux/platform_device.h>
#include <linux/pm_runtime.h>
#include <linux/suspend.h>
#include <linux/unaligned.h>

#include "apple-ane-bringup.h"
#include "apple-ane-ipc.h"
#include "apple-ane-user.h"
#ifdef ANE_HAVE_CASE
#include "apple-ane-case-data.h"
#ifndef ANE_CASE_INPUT_BYTES
#define ANE_CASE_INPUT_BYTES 0x8000U
#define ANE_CASE_OUTPUT_BYTES 0x8000U
#define ANE_CASE_WORK_BYTES 0xcc0U
#define ANE_CASE_ACTION "conv16"
#endif
#ifdef ANE_CASE_ANEMLL
#include "apple-ane-tiny.h"
#endif
#endif

#define ANE_IOVA		0x10000000000ULL
#define ANE_GRANULE	0x4000U
#define ANE_IMAGE_SIZE	0x380000U
#define ANE_ARENA_SIZE	0x1300000U
#define ANE_COMMAND	0x384000U
#define ANE_COMMAND_SIZE 0x40000U
#define ANE_BOOTARGS	0x384180U
#define ANE_IPC		0x3d0000U
#define ANE_IPC_SIZE	0xc441U
#define ANE_HEAP	0x3e4000U
#define ANE_HEAP_SIZE	0xf00000U
#define ANE_READY	0x08042006U
#define ANE_SHARED_LIMIT 0x200000U
#define ANE_SHARED_TOTAL 0xc00000U
#define ANE_FW_NAME	"apple/ane/j700-25G83.flat"

struct ane_buffer {
	struct list_head list;
	void *cpu;
	dma_addr_t dma;
	u64 iova;
	u32 size;
};

struct ane_channel {
	const char *name;
	u32 direction, source, count, offset;
};

static const struct ane_channel ane_channels[] = {
	{ "TERMINAL", 2, 0, 512, 0x800 },
	{ "IO", 0, 1, 16, 0x8800 },
	{ "DEBUG", 0, 2, 8, 0x8c00 },
	{ "BUF_H2T", 0, 3, 64, 0x8e00 },
	{ "BUF_T2H", 1, 4, 64, 0x9e00 },
	{ "SHAREDMALLOC", 1, 5, 8, 0xae00 },
	{ "IO_T2H", 1, 6, 64, 0xb000 },
	{ "DATA_CHAIN_H2T", 0, 7, 16, 0xc000 },
};

static const u64 ane_mmio_ranges[][2] = {
	{ 0x321668000ULL, 0x321668003ULL },
	{ 0x30070c000ULL, 0x30070c013ULL },
	{ 0x300700290ULL, 0x300700293ULL },
	{ 0x482668000ULL, 0x482668003ULL },
};

static const phys_addr_t ane_trad_bases[] = {
	0x401800000ULL, 0x401820000ULL, 0x401840000ULL,
};

struct ane_fw_test {
	struct device *dev, *power, *memory;
	struct device *dart[3];
	void __iomem *trad[3], *cpu, *vector, *scratch, *doorbell, *irq, *ack, *dapf;
	struct iommu_domain *domain;
	struct ane_buffer *arena;
	struct list_head buffers;
	struct mutex mutex;
	struct notifier_block pm_notifier;
	const char *state;
	u64 next_iova;
	u32 shared_bytes, allocations, malloc_head, io_head, mmio_mapped;
	u32 data_head, event_head;
	u64 work_token;
	int last_error;
	bool claimed, attached, started, notifier_registered;
	bool work_pending, work_begin, work_end;
	bool profile_events, work_profile;
	bool session_mode, user_open, misc_registered;
	u32 work_events, completed_jobs, runtime_state;
	struct miscdevice misc;
	const struct firmware *runtime_case;
	u8 *runtime_input, *runtime_output, *runtime_work;
	u32 samples_passed;
	bool case_ran, shutdown_reply;
};

static int ane_fault_state(struct ane_fw_test *ane)
{
	u32 error;
	unsigned int i;

	for (i = 0; i < ARRAY_SIZE(ane->trad); i++) {
		error = readl(ane->trad[i] + 0x100);
		if (error & BIT(31)) {
			dev_err(ane->dev, "DART%u error=%#x address=%#x:%08x\n", i,
				error, readl(ane->trad[i] + 0x174),
				readl(ane->trad[i] + 0x170));
			return -EIO;
		}
	}
	return 0;
}

/* Coherent memory is allocated through a dedicated direct-DMA device because
 * the ANE device has an explicitly owned, unmanaged IOMMU domain. Using its
 * normal DMA API would allocate IOVAs from a different domain's DMA cookie.
 * The allocator has no firmware node, bus DMA offset or IOMMU. Its physical
 * pages are then mapped into the ANE's domain at checked, fixed IOVAs.
 */
static int ane_alloc(struct ane_fw_test *ane, u64 iova, u32 bytes,
		     struct ane_buffer **result)
{
	struct ane_buffer *buffer, *other;
	phys_addr_t physical;
	unsigned long pfn;
	u32 size;
	int ret;

	if (!bytes || bytes > ANE_ARENA_SIZE || !IS_ALIGNED(iova, ANE_GRANULE) ||
	    iova < ANE_IOVA || iova > ANE_IOVA + 0x4000000ULL - bytes)
		return -EINVAL;
	size = ALIGN(bytes, ANE_GRANULE);
	list_for_each_entry(other, &ane->buffers, list)
		if (iova < other->iova + other->size && other->iova < iova + size)
			return -EEXIST;
	buffer = kzalloc_obj(*buffer);
	if (!buffer)
		return -ENOMEM;
	buffer->cpu = dma_alloc_coherent(ane->memory, size, &buffer->dma, GFP_KERNEL);
	if (!buffer->cpu) {
		ret = -ENOMEM;
		goto free_buffer;
	}
	physical = dma_to_phys(ane->memory, buffer->dma);
	if (!IS_ALIGNED(physical, ANE_GRANULE)) {
		ret = -EINVAL;
		goto free_dma;
	}
	for (pfn = PHYS_PFN(physical); pfn < PHYS_PFN(physical + size); pfn++)
		if (!pfn_valid(pfn)) {
			ret = -EINVAL;
			goto free_dma;
		}
	memset(buffer->cpu, 0, size);
	dma_wmb();
	ret = iommu_map(ane->domain, iova, physical, size,
			IOMMU_READ | IOMMU_WRITE, GFP_KERNEL);
	if (ret)
		goto free_dma;
	buffer->size = size;
	buffer->iova = iova;
	list_add_tail(&buffer->list, &ane->buffers);
	*result = buffer;
	return 0;

free_dma:
	dma_free_coherent(ane->memory, size, buffer->cpu, buffer->dma);
free_buffer:
	kfree(buffer);
	return ret;
}

static int ane_prepare_memory(struct ane_fw_test *ane)
{
	static const u8 expected_sha[SHA256_DIGEST_SIZE] = {
		0x1c, 0x27, 0xf0, 0x0f, 0x0b, 0x33, 0xd6, 0xb9,
		0xcd, 0x34, 0x52, 0x76, 0xbd, 0x6e, 0x01, 0x43,
		0x96, 0xe5, 0xcc, 0xfe, 0x2e, 0x6c, 0xc1, 0xfe,
		0x8b, 0xc7, 0xf9, 0xa7, 0xa1, 0xfe, 0x2f, 0x96,
	};
	const struct firmware *firmware;
	u8 digest[SHA256_DIGEST_SIZE];
	u8 *args;
	u64 page;
	unsigned int i;
	int ret;

	ret = request_firmware(&firmware, ANE_FW_NAME, ane->dev);
	if (ret)
		return ret;
	ret = -EINVAL;
	if (firmware->size != ANE_IMAGE_SIZE)
		goto release;
	sha256(firmware->data, firmware->size, digest);
	if (memcmp(digest, expected_sha, sizeof(digest)))
		goto release;
	ret = ane_alloc(ane, ANE_IOVA, ANE_ARENA_SIZE, &ane->arena);
	if (ret)
		goto release;
	memcpy(ane->arena->cpu, firmware->data, firmware->size);
	args = ane->arena->cpu + ANE_BOOTARGS;
	/* The 0x174-byte native record has zero-filled unmodeled fields.
	 * +0 is zero, +0x12c is the captured opaque value 4.
	 */
	put_unaligned_le64(ANE_IOVA + ANE_IPC, args + 8);
	put_unaligned_le64(ANE_IMAGE_SIZE, args + 0x10);
	put_unaligned_le64(0xfc80000, args + 0x18);
	put_unaligned_le64(ANE_IOVA + ANE_HEAP, args + 0x20);
	put_unaligned_le64(ANE_HEAP_SIZE, args + 0x28);
	put_unaligned_le64(ANE_IPC_SIZE, args + 0x50);
	put_unaligned_le64(ANE_IOVA + ANE_COMMAND, args + 0x58);
	put_unaligned_le64(ANE_COMMAND_SIZE, args + 0x60);
	put_unaligned_le32(64, args + 0x68);
	put_unaligned_le32(4, args + 0x12c);
	dma_wmb();
	/* DAPF restricts these page mappings to the four observed SID-0 ranges. */
	for (i = 0; i < ARRAY_SIZE(ane_mmio_ranges); i++) {
		page = round_down(ane_mmio_ranges[i][0], ANE_GRANULE);
		ret = iommu_map(ane->domain, page, page, ANE_GRANULE,
				IOMMU_READ | IOMMU_WRITE | IOMMU_MMIO, GFP_KERNEL);
		if (ret)
			goto release;
		ane->mmio_mapped++;
	}
release:
	release_firmware(firmware);
	return ret;
}

static int ane_prepare_dapf(struct ane_fw_test *ane)
{
	static const u32 offsets[] = { 8, 12, 16, 20, 32, 0 };
	static const u32 masks[] = { 0xfffffffc, 0xf, 0xfffffffc, 0xf, 0xffff, 0x333 };
	u32 expected[4][6], protect, lock;
	unsigned int i, j, mismatch = 0;

	/* This test only qualifies v2.3 with a single SID bitmap word. */
	if ((readl(ane->trad[0] + 8) & 0xffff) != 0x203 ||
	    !(readl(ane->trad[0] + 12) & 0x1ff) ||
	    (readl(ane->trad[0] + 12) & 0x1ff) > 32)
		return -EOPNOTSUPP;
	protect = readl(ane->trad[0] + 0x200);
	lock = readl(ane->trad[0] + 0x208);
	for (i = 0; i < ARRAY_SIZE(ane_mmio_ranges); i++) {
		expected[i][0] = lower_32_bits(ane_mmio_ranges[i][0]);
		expected[i][1] = upper_32_bits(ane_mmio_ranges[i][0]);
		expected[i][2] = lower_32_bits(ane_mmio_ranges[i][1]);
		expected[i][3] = upper_32_bits(ane_mmio_ranges[i][1]);
		expected[i][4] = 1;
		expected[i][5] = 0x31;
		for (j = 0; j < ARRAY_SIZE(offsets); j++)
			if ((readl(ane->dapf + i * 0x40 + offsets[j]) & masks[j]) !=
			    (expected[i][j] & masks[j]))
				mismatch++;
	}
	if ((protect & lock & BIT(2)) && mismatch)
		return -EPERM;
	if (readl(ane->trad[0] + 0x200) != protect ||
	    readl(ane->trad[0] + 0x208) != lock)
		return -EBUSY;
	if (mismatch)
		for (i = 0; i < ARRAY_SIZE(ane_mmio_ranges); i++)
			for (j = 0; j < ARRAY_SIZE(offsets); j++) {
				writel(expected[i][j], ane->dapf + i * 0x40 + offsets[j]);
				if ((readl(ane->dapf + i * 0x40 + offsets[j]) & masks[j]) !=
				    (expected[i][j] & masks[j]))
					return -EIO;
			}
	if (readl(ane->trad[0] + 0x200) != protect ||
	    readl(ane->trad[0] + 0x208) != lock)
		return -EBUSY;
	dev_info(ane->dev, "DAPF qualified: four SID-0 ranges, changed=%u protect=%#x lock=%#x\n",
		 mismatch, protect, lock);
	return 0;
}

static int ane_poll_scratch(struct ane_fw_test *ane, u32 offset, u32 expected)
{
	unsigned long deadline = jiffies + msecs_to_jiffies(3000);
	u32 value;
	int ret;

	do {
		ret = ane_fault_state(ane);
		if (ret)
			return ret;
		value = readl(ane->scratch + offset);
		if (value == expected)
			return 0;
		usleep_range(100, 200);
	} while (time_before(jiffies, deadline));
	dev_err(ane->dev, "scratch +%#x timed out: got=%#x wanted=%#x\n",
		offset, value, expected);
	return -ETIMEDOUT;
}

static int ane_validate_channels(struct ane_fw_test *ane)
{
	u8 *ipc = ane->arena->cpu + ANE_IPC;
	const struct ane_channel *channel;
	struct apple_ane_ipc_slot *slot;
	unsigned int i, j;
	u8 *descriptor;

	dma_rmb();
	/* Validate every descriptor before modifying any queue. */
	for (i = 0; i < ARRAY_SIZE(ane_channels); i++) {
		channel = &ane_channels[i];
		descriptor = ipc + i * 0x100;
		if (strnlen((const char *)descriptor, 64) != strlen(channel->name) ||
		    memcmp(descriptor, channel->name, strlen(channel->name)) ||
		    get_unaligned_le32(descriptor + 0x40) != channel->direction ||
		    get_unaligned_le32(descriptor + 0x44) != channel->source ||
		    get_unaligned_le64(descriptor + 0x48) != channel->count ||
		    get_unaligned_le64(descriptor + 0x50) !=
			ANE_IOVA + ANE_IPC + channel->offset)
			return -EPROTO;
	}
	for (i = 0; i < ARRAY_SIZE(ane_channels); i++) {
		channel = &ane_channels[i];
		if (channel->direction)
			continue;
		slot = (void *)(ipc + channel->offset);
		for (j = 0; j < channel->count; j++) {
			WRITE_ONCE(slot[j].length, cpu_to_le64(0));
			WRITE_ONCE(slot[j].metadata, cpu_to_le64(0));
			WRITE_ONCE(slot[j].pointer, cpu_to_le64(1));
		}
	}
	dma_wmb();
	return 0;
}

static int ane_handshake(struct ane_fw_test *ane)
{
	u64 pointer = ANE_IOVA + ANE_BOOTARGS;
	u32 offset;
	int ret;

	/* Pin before the first CPU-control write; no failure can release DMA. */
	__module_get(THIS_MODULE);
	ane->started = true;
	ane->state = "firmware-wake";
	for (offset = 0; offset < 0x20; offset += 4)
		writel(0, ane->scratch + offset);
	writel(1, ane->scratch + 0x18);
	writel(0, ane->scratch + 0x1c);
	writel(0, ane->cpu + 4);
	writel(0x10, ane->cpu + 4);
	ret = ane_poll_scratch(ane, 0x1c, ANE_READY);
	if (ret)
		return ret;
	if (readl(ane->scratch) != ARRAY_SIZE(ane_channels) ||
	    readl(ane->scratch + 4) != ANE_IPC_SIZE - 1 ||
	    readl(ane->scratch + 0xc) != ANE_HEAP_SIZE ||
	    readl(ane->scratch + 0x10))
		return -EPROTO;
	ane->state = "firmware-bootargs";
	writel(lower_32_bits(pointer), ane->scratch);
	writel(upper_32_bits(pointer), ane->scratch + 4);
	writel(~ANE_READY, ane->scratch + 0x1c);
	ret = ane_poll_scratch(ane, 0x1c, ANE_READY);
	if (ret)
		return ret;
	if (readl(ane->scratch) != ANE_IPC ||
	    readl(ane->scratch + 4) != upper_32_bits(ANE_IOVA))
		return -EPROTO;
	ret = ane_validate_channels(ane);
	if (ret)
		return ret;
	writel(ANE_READY, ane->scratch + 0xc);
	ret = ane_poll_scratch(ane, 0xc, 0);
	if (!ret)
		dev_info(ane->dev, "ANE firmware handshake: eight validated channels\n");
	return ret;
}

static int ane_service_malloc(struct ane_fw_test *ane)
{
	struct apple_ane_ipc_slot *ring = ane->arena->cpu + ANE_IPC + 0xae00;
	struct apple_ane_ipc_slot *slot = &ring[ane->malloc_head];
	struct ane_buffer *allocation;
	u32 size, rounded;
	u64 tag;
	int ret;

	if (le64_to_cpu(READ_ONCE(slot->pointer)) & 1)
		return 0;
	dma_rmb();
	if (!le64_to_cpu(READ_ONCE(slot->pointer)) &&
	    !le64_to_cpu(READ_ONCE(slot->length)))
		return 0;
	ret = apple_ane_alloc_request(slot, ANE_SHARED_LIMIT, &size, &tag);
	if (ret) {
		dev_err(ane->dev, "shared request rejected: error=%d size=%#llx tag=%#llx\n",
			ret, le64_to_cpu(READ_ONCE(slot->length)), le64_to_cpu(READ_ONCE(slot->metadata)));
		return ret;
	}
	/* Native program-create (0x202) requests a 2 MiB EXPP pool. Retain the
	 * smaller bound for every other request until its form is qualified.
	 */
	if (size > 0x100000 && (size != 0x200000 || tag != 0x45585050))
		return -EOPNOTSUPP;
	rounded = ALIGN(size, ANE_GRANULE);
	if (rounded > ANE_SHARED_TOTAL - ane->shared_bytes || ane->allocations >= 64)
		return -ENOSPC;
	ret = ane_alloc(ane, ane->next_iova, rounded, &allocation);
	if (ret)
		return ret;
	ret = apple_ane_alloc_reply(slot, size, tag, allocation->iova, ane->allocations + 1);
	if (ret)
		return ret;
	ane->next_iova += rounded;
	ane->shared_bytes += rounded;
	ane->allocations++;
	ane->malloc_head = (ane->malloc_head + 1) % 8;
	writel(BIT(5), ane->doorbell);
	dev_info(ane->dev, "shared allocation %u: tag=%#llx size=%#x iova=%#llx\n",
		 ane->allocations, tag, size, allocation->iova);
	return 0;
}

static void *ane_buffer_pointer(struct ane_fw_test *ane, u64 iova, u32 size)
{
	struct ane_buffer *buffer;

	if (!size || iova >= BIT_ULL(42) || size > BIT_ULL(42) - iova)
		return NULL;
	list_for_each_entry(buffer, &ane->buffers, list)
		if (iova >= buffer->iova && iova - buffer->iova < buffer->size &&
		    size <= buffer->size - (iova - buffer->iova))
			return buffer->cpu + (iova - buffer->iova);
	return NULL;
}

static int ane_service_work(struct ane_fw_test *ane)
{
	struct apple_ane_ipc_slot *ring = ane->arena->cpu + ANE_IPC + 0xb000;
	struct apple_ane_ipc_slot *slot = &ring[ane->event_head];
	u64 pointer = le64_to_cpu(READ_ONCE(slot->pointer));
	u8 payload[96];
	void *source;
	bool completed = false, profile;
	u32 stage;
	int ret;

	if (pointer & 1)
		return 0;
	dma_rmb();
	if (!pointer && !le64_to_cpu(READ_ONCE(slot->length)))
		return 0;
	if (!ane->work_pending || !pointer || (pointer & 3) ||
	    le64_to_cpu(READ_ONCE(slot->length)) != sizeof(payload) ||
	    le64_to_cpu(READ_ONCE(slot->metadata)))
		return -EPROTO;
	source = ane_buffer_pointer(ane, pointer, sizeof(payload));
	if (!source)
		return -ERANGE;
	memcpy(payload, source, sizeof(payload));
	/* This experiment has at most three jobs. Message handles
	 * are reused independently of this ring's consumer position.
	 */
	if (!ane->session_mode && ane->event_head >= (ane->profile_events ? 9 : 6))
		return -EOVERFLOW;
	profile = get_unaligned_le32(payload + 0x1c) == 3;
	if (ane->session_mode) {
		ret = apple_ane_session_event(payload, sizeof(payload), ane->work_token, &stage);
		if (!ret)
			completed = stage == 1;
	} else if (profile && ane->profile_events && ane->work_token >= 1 && ane->work_token <= 3)
		ret = apple_ane_profile_event(payload, sizeof(payload), ane->work_token,
					     ane->work_token - 1);
	else
		ret = apple_ane_work_event(payload, sizeof(payload), ane->work_token, &completed);
	if (ret) {
		dev_err(ane->dev, "work message rejected: error=%d id=%#x opcode=%#x status=%#x token=%llu stage=%u\n",
			ret, get_unaligned_le32(payload), get_unaligned_le16(payload + 4),
			get_unaligned_le16(payload + 6), get_unaligned_le64(payload + 8),
			get_unaligned_le32(payload + 0x1c));
		return ret;
	}
	if (profile) {
		if (!ane->work_begin || ane->work_end || ane->work_profile)
			return -EPROTO;
		ane->work_profile = true;
	} else {
		if (completed ? (!ane->work_begin || ane->work_end ||
				 (ane->profile_events && !ane->work_profile)) : ane->work_begin)
			return -EPROTO;
		if (completed)
			ane->work_end = true;
		else
			ane->work_begin = true;
	}
	/* Native consumption changes only ownership, retaining length/metadata. */
	dma_wmb();
	WRITE_ONCE(slot->pointer, cpu_to_le64(pointer | 1));
	dma_wmb();
	writel(BIT(6), ane->doorbell);
	ane->event_head = (ane->event_head + 1) % 64;
	ane->work_events++;
	dev_info(ane->dev, "ANE work token=%llu %s\n", ane->work_token,
		 profile ? "profiled" : completed ? "completed" : "began");
	return 0;
}

static int ane_service(struct ane_fw_test *ane)
{
	u32 status;
	int ret;

	ret = ane_fault_state(ane);
	if (ret)
		return ret;
	status = readl(ane->irq);
	if (status & ~0xffU)
		return -EPROTO;
	if (status)
		writel(status, ane->ack);
	ret = ane_service_malloc(ane);
	return ret ? ret : ane_service_work(ane);
}

static int ane_command(struct ane_fw_test *ane, bool data_chain,
		       const void *request, u32 size)
{
	struct apple_ane_ipc_slot *ring = ane->arena->cpu + ANE_IPC +
		(data_chain ? 0xc000 : 0x8800);
	struct apple_ane_io_transaction transaction = {};
	/* Keep the live 0x174-byte boot record at command pool +0x180 intact. */
	u8 *command = ane->arena->cpu + ANE_COMMAND + 0x4000;
	unsigned long deadline = jiffies + msecs_to_jiffies(3000);
	u32 *head = data_chain ? &ane->data_head : &ane->io_head;
	u32 opcode, length;
	u64 metadata;
	int ret;

	if (!request || size < 8 || size > 4096 || get_unaligned_le32(request))
		return -EINVAL;
	opcode = get_unaligned_le32(request + 4);
	memcpy(command, request, size);
	ret = apple_ane_io_publish(&transaction, &ring[*head],
				   ANE_IOVA + ANE_COMMAND + 0x4000, size);
	if (ret)
		return ret;
	writel(data_chain ? BIT(7) : BIT(1), ane->doorbell);
	do {
		ret = ane_service(ane);
		if (ret)
			return ret;
		ret = apple_ane_io_poll(&transaction, &length, &metadata);
		if (!ret) {
			if (length != size || metadata || get_unaligned_le32(command) ||
			    get_unaligned_le32(command + 4) != opcode)
				return -EPROTO;
			*head = (*head + 1) % 16;
			dev_info(ane->dev, "ANE %s opcode=%#x reply=%u status=0\n",
				 data_chain ? "DATA_CHAIN" : "IO", opcode, length);
			return 0;
		}
		if (ret != -EAGAIN)
			return ret;
		usleep_range(100, 200);
	} while (time_before(jiffies, deadline));
	return -ETIMEDOUT;
}

static int ane_startup_command(struct ane_fw_test *ane)
{
	u8 request[12] = {};
	int ret;

	put_unaligned_le32(4, request + 4);
	ret = ane_command(ane, false, request, sizeof(request));
	if (!ret && get_unaligned_le32(ane->arena->cpu + ANE_COMMAND + 0x4008))
		return -EPROTO;
	return ret;
}

/* Used only before RUN. Once started, all allocations and power holds stay. */
static void ane_release_memory(struct ane_fw_test *ane)
{
	struct ane_buffer *buffer, *next;
	unsigned int i;
	u64 page;

	if (WARN_ON(ane->started))
		return;
	if (ane->domain) {
		for (i = 0; i < ane->mmio_mapped; i++) {
			page = round_down(ane_mmio_ranges[i][0], ANE_GRANULE);
			iommu_unmap(ane->domain, page, ANE_GRANULE);
		}
		list_for_each_entry_safe(buffer, next, &ane->buffers, list) {
			iommu_unmap(ane->domain, buffer->iova, buffer->size);
			dma_free_coherent(ane->memory, buffer->size, buffer->cpu, buffer->dma);
			list_del(&buffer->list);
			kfree(buffer);
		}
		if (ane->attached)
			iommu_detach_device(ane->domain, ane->dev);
		iommu_domain_free(ane->domain);
		ane->domain = NULL;
	}
	if (ane->claimed) {
		iommu_device_release_dma_owner(ane->dev);
		ane->claimed = false;
	}
	ane->attached = false;
	ane->mmio_mapped = 0;
	ane->arena = NULL;
}

static int ane_start_locked(struct ane_fw_test *ane)
{
	struct device *dev = ane->dev;
	const char *failed_stage;
	int ret;

	if (strcmp(ane->state, "ready")) {
		ret = -EBUSY;
		goto unlock;
	}
	ane->state = "preparing";
	ret = -EBUSY;
	if (readl(ane->cpu + 4) || readq(ane->vector) != (ANE_IOVA | 1))
		goto fail;
	ret = ane_fault_state(ane);
	if (ret)
		goto fail;
	ane->state = "claiming-dma";
	ret = iommu_device_claim_dma_owner(dev, ane);
	if (ret)
		goto fail;
	ane->claimed = true;
	ane->state = "allocating-domain";
	ane->domain = iommu_paging_domain_alloc(dev);
	if (IS_ERR(ane->domain)) {
		ret = PTR_ERR(ane->domain);
		ane->domain = NULL;
		goto fail;
	}
	ane->state = "attaching-domain";
	ret = iommu_attach_device(ane->domain, dev);
	if (ret)
		goto fail;
	ane->attached = true;
	if (!(ane->domain->pgsize_bitmap & ANE_GRANULE) ||
	    ane->domain->geometry.aperture_end < ANE_IOVA + 0x4000000ULL - 1 ||
	    ane->domain->geometry.aperture_start > 0x300700000ULL) {
		ret = -EOPNOTSUPP;
		goto fail;
	}
	ane->state = "mapping-firmware";
	ret = ane_prepare_memory(ane);
	if (ret)
		goto fail;
	ane->state = "validating-dapf";
	ret = ane_prepare_dapf(ane);
	if (ret)
		goto fail;
	ret = ane_handshake(ane);
	if (ret)
		goto fail;
	ane->state = "startup-command";
	ret = ane_startup_command(ane);
	if (ret)
		goto fail;
	ane->state = "startup-complete";
	dev_info(dev, "ANE startup command completed; numerical qualification pending\n");
	goto unlock;
fail:
	failed_stage = ane->state;
	ane->last_error = ret;
	if (ane->started) {
		ane->state = "fault-retained";
		dev_err(dev, "ANE %s error %d; DMA and power retained until Neo reboot\n",
			failed_stage, ret);
	} else {
		ane_release_memory(ane);
		ane->state = "preflight-failed";
		dev_err(dev, "ANE preflight %s error %d; firmware was not started\n",
			failed_stage, ret);
	}
unlock:
	return ret;
}

static ssize_t start_bootstrap_store(struct device *dev, struct device_attribute *attr,
				     const char *buf, size_t count)
{
	struct ane_fw_test *ane = dev_get_drvdata(dev);
	int ret;

	if (!sysfs_streq(buf, "25G83"))
		return -EINVAL;
	mutex_lock(&ane->mutex);
	ret = ane->user_open ? -EBUSY : ane_start_locked(ane);
	mutex_unlock(&ane->mutex);
	return ret ? ret : count;
}
static DEVICE_ATTR_WO(start_bootstrap);

#ifdef ANE_HAVE_CASE
static const u8 *ane_case_data(const struct firmware *fw, u32 offset, u32 size)
{
	if (!size || offset > fw->size || size > fw->size - offset)
		return NULL;
	return fw->data + offset;
}

static int ane_case_map(struct ane_fw_test *ane, u64 iova, u32 size,
			const u8 *contents, void **result)
{
	struct ane_buffer *buffer;
	u64 base = round_down(iova, ANE_GRANULE);
	void *pointer = ane_buffer_pointer(ane, iova, size);
	int ret;

	if (!pointer) {
		ret = ane_alloc(ane, base, ALIGN(iova - base + size, ANE_GRANULE), &buffer);
		if (ret)
			return ret;
		pointer = ane_buffer_pointer(ane, iova, size);
		if (!pointer)
			return -ERANGE;
	}
	if (contents)
		memcpy(pointer, contents, size);
	if (result)
		*result = pointer;
	return 0;
}

static int ane_run_case(struct ane_fw_test *ane, const struct firmware *fw)
{
	u8 digest[SHA256_DIGEST_SIZE], gate[12] = {};
	u8 *input, *output, *work;
	const u8 *contents, *expected;
	unsigned long deadline;
	unsigned int i, j, mismatch;
	int ret;
#ifdef ANE_CASE_ANEMLL
	u32 tokens[32] = { 1, 403, 407, 261, 378 }, token_count = 5, next_token;
	const u8 *embeddings;
#endif

	/* This build accepts exactly one locally validated own-model package.
	 * CPU reference bytes stay outside the ANE IOMMU domain at all times.
	 */
	if (fw->size != ANE_CASE_SIZE)
		return -EINVAL;
	sha256(fw->data, fw->size, digest);
	if (memcmp(digest, ane_case_sha, sizeof(digest)))
		return -EINVAL;
#ifdef ANE_CASE_ANEMLL
	embeddings = ane_case_data(fw, ANE_CASE_EMBEDDINGS, 512 * 64 * 2);
	if (!embeddings || ARRAY_SIZE(ane_case_samples) != 3)
		return -EINVAL;
	ane->profile_events = true;
#endif
	ane->state = "mapping-case";
	for (i = 0; i < ARRAY_SIZE(ane_case_buffers); i++) {
		contents = ane_case_data(fw, ane_case_buffers[i].offset, ane_case_buffers[i].size);
		if (!contents)
			return -ERANGE;
		ret = ane_case_map(ane, ane_case_buffers[i].iova,
				   ane_case_buffers[i].size, contents, NULL);
		if (ret)
			return ret;
	}
	ret = ane_case_map(ane, ANE_CASE_INPUT, ANE_CASE_INPUT_BYTES, NULL, (void **)&input);
	if (ret)
		return ret;
	ret = ane_case_map(ane, ANE_CASE_OUTPUT, ANE_CASE_OUTPUT_BYTES, NULL, (void **)&output);
	if (ret)
		return ret;
	dma_wmb();
	ane->state = "loading-program";
	for (i = 0; i < ARRAY_SIZE(ane_case_commands); i++) {
		contents = ane_case_data(fw, ane_case_commands[i].offset, ane_case_commands[i].size);
		if (!contents)
			return -ERANGE;
		ret = ane_command(ane, false, contents, ane_case_commands[i].size);
		if (ret)
			return ret;
	}
	contents = ane_case_data(fw, ane_case_work[0].offset, ane_case_work[0].size);
	if (!contents || ane_case_work[0].size != ANE_CASE_WORK_BYTES)
		return -ERANGE;
	work = kmemdup(contents, ane_case_work[0].size, GFP_KERNEL);
	if (!work)
		return -ENOMEM;
	put_unaligned_le32(0x2d, gate + 4);
	for (i = 0; i < ARRAY_SIZE(ane_case_samples); i++) {
		ane->state = "executing-sample";
		contents = ane_case_data(fw, ane_case_samples[i].input, ANE_CASE_INPUT_BYTES);
		expected = ane_case_data(fw, ane_case_samples[i].expected, ANE_CASE_OUTPUT_BYTES);
		if (!contents || !expected) {
			ret = -ERANGE;
			goto free_work;
		}
#ifdef ANE_CASE_ANEMLL
		ret = apple_ane_tiny_input(embeddings, 512 * 64 * 2, tokens, token_count,
					   input, ANE_CASE_INPUT_BYTES);
		if (ret)
			goto free_work;
		if (memcmp(input, contents, ANE_CASE_INPUT_BYTES)) {
			ret = -EILSEQ;
			goto free_work;
		}
#else
		memcpy(input, contents, ANE_CASE_INPUT_BYTES);
#endif
		/* Poison every output, so stale results and unwritten pages fail. */
		for (j = 0; j < ANE_CASE_OUTPUT_BYTES / 2; j++)
			put_unaligned_le16(0x7e00, output + j * 2);
		dma_wmb();
		put_unaligned_le32(0, gate + 8);
		ret = ane_command(ane, true, gate, sizeof(gate));
		if (ret)
			goto free_work;
		ane->work_token = i + 1;
		ane->work_begin = false;
		ane->work_end = false;
		ane->work_profile = false;
		ane->work_pending = true;
		put_unaligned_le64(ane->work_token, work + 0x20);
		ret = ane_command(ane, true, work, ane_case_work[0].size);
		if (ret)
			goto free_work;
		deadline = jiffies + msecs_to_jiffies(3000);
		while (!ane->work_end) {
			ret = ane_service(ane);
			if (ret)
				goto free_work;
			if (time_after_eq(jiffies, deadline)) {
				ret = -ETIMEDOUT;
				goto free_work;
			}
			usleep_range(100, 200);
		}
		ane->work_pending = false;
		put_unaligned_le32(1, gate + 8);
		ret = ane_command(ane, true, gate, sizeof(gate));
		if (ret)
			goto free_work;
		dma_rmb();
		mismatch = 0;
		for (j = 0; j < ANE_CASE_OUTPUT_BYTES / 2; j++) {
			if (get_unaligned_le16(output + j * 2) == get_unaligned_le16(expected + j * 2))
				continue;
			if (mismatch++ < 4)
				dev_err(ane->dev, "sample %u output[%u]=%#x expected=%#x\n", i, j,
					get_unaligned_le16(output + j * 2), get_unaligned_le16(expected + j * 2));
		}
		sha256(output, ANE_CASE_OUTPUT_BYTES, digest);
		dev_info(ane->dev, "ANE sample=%u token=%llu values=%u mismatches=%u sha256=%*phN\n",
			 i, ane->work_token, ANE_CASE_OUTPUT_BYTES / 2, mismatch, SHA256_DIGEST_SIZE, digest);
		if (mismatch) {
			ret = -EILSEQ;
			goto free_work;
		}
#ifdef ANE_CASE_ANEMLL
		ret = apple_ane_tiny_argmax(output, ANE_CASE_OUTPUT_BYTES, token_count - 1, &next_token);
		if (ret)
			goto free_work;
		dev_info(ane->dev, "ANE decoded sample=%u context=%u next_token=%u\n",
			 i, token_count, next_token);
		if (next_token != ane_case_next_tokens[i] || token_count >= ARRAY_SIZE(tokens)) {
			ret = -EILSEQ;
			goto free_work;
		}
		tokens[token_count++] = next_token;
#endif
		ane->samples_passed++;
	}
	ane->state = "closing-program";
	for (i = 0; i < ARRAY_SIZE(ane_case_teardown); i++) {
		contents = ane_case_data(fw, ane_case_teardown[i].offset, ane_case_teardown[i].size);
		if (!contents) {
			ret = -ERANGE;
			goto free_work;
		}
		ret = ane_command(ane, false, contents, ane_case_teardown[i].size);
		if (ret)
			goto free_work;
	}
	ane->shutdown_reply = true;
	ane->state = "computation-complete-retained";
	dev_info(ane->dev, "ANE three numerical samples passed and shutdown replied; DMA/power retained until Neo reboot\n");
free_work:
	/* This is only the CPU request copy. Every DMA allocation remains held. */
	kfree(work);
	return ret;
}

static ssize_t test_compute_store(struct device *dev, struct device_attribute *attr,
				 const char *buf, size_t count)
{
	struct ane_fw_test *ane = dev_get_drvdata(dev);
	const struct firmware *fw;
	const char *failed_stage;
	int ret;

	if (!sysfs_streq(buf, ANE_CASE_ACTION))
		return -EINVAL;
	mutex_lock(&ane->mutex);
	if (strcmp(ane->state, "startup-complete") || ane->case_ran || ane->user_open) {
		ret = -EBUSY;
		goto unlock;
	}
	ret = request_firmware(&fw, ANE_CASE_NAME, dev);
	if (ret)
		goto unlock;
	ane->case_ran = true;
	ret = ane_run_case(ane, fw);
	release_firmware(fw);
	if (ret) {
		failed_stage = ane->state;
		ane->state = "case-fault-retained";
		ane->last_error = ret;
		dev_err(dev, "ANE case %s error=%d samples_passed=%u; retain DMA/power until Neo reboot\n",
			failed_stage, ret, ane->samples_passed);
	}
unlock:
	mutex_unlock(&ane->mutex);
	return ret ? ret : count;
}
static DEVICE_ATTR_WO(test_compute);
MODULE_FIRMWARE(ANE_CASE_NAME);
#endif

#ifdef ANE_CASE_ANEMLL
static void ane_runtime_fail(struct ane_fw_test *ane, int error)
{
	ane->last_error = error;
	ane->runtime_state = AURORA_ANE_STATE_FAULT;
	ane->state = "runtime-fault-retained";
	dev_err(ane->dev, "runtime stopped: error=%d completed=%u events=%u; retain DMA/power until Neo reboot\n",
		error, ane->completed_jobs, ane->work_events);
}

static void ane_runtime_drop_cpu(struct ane_fw_test *ane)
{
	/* Only CPU copies: no mapped memory is freed after RUN. */
	kfree(ane->runtime_work);
	ane->runtime_work = NULL;
	if (ane->runtime_case)
		release_firmware(ane->runtime_case);
	ane->runtime_case = NULL;
}

static int ane_runtime_load(struct ane_fw_test *ane)
{
	const struct firmware *fw;
	const u8 *data;
	u8 digest[SHA256_DIGEST_SIZE];
	unsigned int i;
	int ret;

	if (ane->runtime_state == AURORA_ANE_STATE_FAULT)
		return -EIO;
	if (ane->case_ran || (strcmp(ane->state, "ready") && strcmp(ane->state, "startup-complete")))
		return -EBUSY;
	ret = request_firmware(&fw, ANE_CASE_NAME, ane->dev);
	if (ret)
		return ret;
	sha256(fw->data, fw->size, digest);
	data = ane_case_data(fw, ane_case_work[0].offset, ane_case_work[0].size);
	if (fw->size != ANE_CASE_SIZE || memcmp(digest, ane_case_sha, sizeof(digest)) ||
	    !data || ane_case_work[0].size != ANE_CASE_WORK_BYTES) {
		release_firmware(fw);
		return -EINVAL;
	}
	ane->runtime_work = kmemdup(data, ANE_CASE_WORK_BYTES, GFP_KERNEL);
	if (!ane->runtime_work) {
		release_firmware(fw);
		return -ENOMEM;
	}
	ane->runtime_case = fw;
	ane->case_ran = true;
	ane->session_mode = true;
	if (!strcmp(ane->state, "ready")) {
		ret = ane_start_locked(ane);
		if (ret)
			goto fail;
	}
	ane->state = "runtime-mapping";
	for (i = 0; i < ARRAY_SIZE(ane_case_buffers); i++) {
		data = ane_case_data(fw, ane_case_buffers[i].offset, ane_case_buffers[i].size);
		if (!data) {
			ret = -ERANGE;
			goto fail;
		}
		ret = ane_case_map(ane, ane_case_buffers[i].iova, ane_case_buffers[i].size, data, NULL);
		if (ret)
			goto fail;
	}
	ret = ane_case_map(ane, ANE_CASE_INPUT, ANE_CASE_INPUT_BYTES, NULL, (void **)&ane->runtime_input);
	if (ret)
		goto fail;
	ret = ane_case_map(ane, ANE_CASE_OUTPUT, ANE_CASE_OUTPUT_BYTES, NULL, (void **)&ane->runtime_output);
	if (ret)
		goto fail;
	dma_wmb();
	ane->state = "runtime-loading";
	for (i = 0; i < ARRAY_SIZE(ane_case_commands); i++) {
		data = ane_case_data(fw, ane_case_commands[i].offset, ane_case_commands[i].size);
		if (!data) {
			ret = -ERANGE;
			goto fail;
		}
		ret = ane_command(ane, false, data, ane_case_commands[i].size);
		if (ret)
			goto fail;
	}
	ane->profile_events = true;
	ane->runtime_state = AURORA_ANE_STATE_LOADED;
	ane->state = "runtime-loaded";
	return 0;
fail:
	ane_runtime_fail(ane, ret);
	ane_runtime_drop_cpu(ane);
	return ret;
}

static int ane_runtime_execute(struct ane_fw_test *ane, const u8 *input)
{
	u8 gate[12] = {};
	unsigned long deadline;
	u32 j, events = ane->work_events;
	int ret;

	memcpy(ane->runtime_input, input, ANE_CASE_INPUT_BYTES);
	for (j = 0; j < ANE_CASE_OUTPUT_BYTES / 2; j++)
		put_unaligned_le16(0x7e00, ane->runtime_output + j * 2);
	dma_wmb();
	put_unaligned_le32(0x2d, gate + 4);
	ret = ane_command(ane, true, gate, sizeof(gate));
	if (ret)
		goto fail;
	ane->work_token = ane->completed_jobs + 1;
	ane->work_begin = false;
	ane->work_end = false;
	ane->work_profile = false;
	ane->work_pending = true;
	put_unaligned_le64(ane->work_token, ane->runtime_work + 0x20);
	ret = ane_command(ane, true, ane->runtime_work, ANE_CASE_WORK_BYTES);
	if (ret)
		goto fail;
	deadline = jiffies + msecs_to_jiffies(3000);
	while (!ane->work_end) {
		ret = ane_service(ane);
		if (ret)
			goto fail;
		if (time_after_eq(jiffies, deadline)) {
			ret = -ETIMEDOUT;
			goto fail;
		}
		usleep_range(100, 200);
	}
	ane->work_pending = false;
	put_unaligned_le32(1, gate + 8);
	ret = ane_command(ane, true, gate, sizeof(gate));
	if (ret)
		goto fail;
	dma_rmb();
	if (ane->work_events - events != 3) {
		ret = -EPROTO;
		goto fail;
	}
	for (j = 0; j < ANE_CASE_OUTPUT_BYTES / 2; j++)
		if ((get_unaligned_le16(ane->runtime_output + j * 2) & 0x7c00) == 0x7c00) {
			ret = -EILSEQ;
			goto fail;
		}
	ane->completed_jobs++;
	return 0;
fail:
	ane_runtime_fail(ane, ret);
	return ret;
}

static int ane_runtime_shutdown(struct ane_fw_test *ane)
{
	const u8 *data;
	unsigned int i;
	int ret;

	if (ane->runtime_state == AURORA_ANE_STATE_CLOSED)
		return 0;
	if (ane->runtime_state != AURORA_ANE_STATE_LOADED)
		return ane->runtime_state == AURORA_ANE_STATE_FAULT ? -EIO : -EINVAL;
	for (i = 0; i < ARRAY_SIZE(ane_case_teardown); i++) {
		data = ane_case_data(ane->runtime_case, ane_case_teardown[i].offset, ane_case_teardown[i].size);
		ret = data ? ane_command(ane, false, data, ane_case_teardown[i].size) : -ERANGE;
		if (ret) {
			ane_runtime_fail(ane, ret);
			return ret;
		}
	}
	ane->shutdown_reply = true;
	ane->runtime_state = AURORA_ANE_STATE_CLOSED;
	ane->state = "runtime-closed-retained";
	ane_runtime_drop_cpu(ane);
	dev_info(ane->dev, "runtime closed: jobs=%u events=%u; DMA/power retained until Neo reboot\n",
		 ane->completed_jobs, ane->work_events);
	return 0;
}

static int ane_user_request_valid(const struct aurora_ane_run *run)
{
	if (run->abi != AURORA_ANE_ABI || run->flags ||
	    run->input_bytes != ANE_CASE_INPUT_BYTES || run->output_bytes != ANE_CASE_OUTPUT_BYTES ||
	    !run->input || !run->output || run->sequence || run->elapsed_ns ||
	    run->reserved[0] || run->reserved[1])
		return -EINVAL;
	return 0;
}

static long ane_user_ioctl(struct file *file, unsigned int command, unsigned long argument)
{
	struct ane_fw_test *ane = file->private_data;
	void __user *arg = (void __user *)argument;
	struct aurora_ane_info info = {};
	struct aurora_ane_run run;
	u8 *input;
	u64 start;
	u32 i;
	int ret;

	if (!capable(CAP_SYS_RAWIO))
		return -EPERM;
	if (command != AURORA_ANE_INFO && !(file->f_mode & FMODE_WRITE))
		return -EBADF;
	ret = mutex_lock_interruptible(&ane->mutex);
	if (ret)
		return ret;
	switch (command) {
	case AURORA_ANE_INFO:
		info.abi = AURORA_ANE_ABI;
		info.model = AURORA_ANE_MODEL_TINY260K;
		info.input_bytes = ANE_CASE_INPUT_BYTES;
		info.output_bytes = ANE_CASE_OUTPUT_BYTES;
		info.max_jobs = AURORA_ANE_MAX_JOBS;
		info.completed_jobs = ane->completed_jobs;
		info.state = ane->runtime_state;
		info.flags = ane->started ? AURORA_ANE_RESET_REQUIRED : 0;
		info.last_error = ane->last_error;
		info.events = ane->work_events;
		memcpy(info.case_sha256, ane_case_sha, sizeof(info.case_sha256));
		ret = copy_to_user(arg, &info, sizeof(info)) ? -EFAULT : 0;
		break;
	case AURORA_ANE_LOAD:
		ret = argument ? -EINVAL : ane_runtime_load(ane);
		break;
	case AURORA_ANE_SHUTDOWN:
		ret = argument ? -EINVAL : ane_runtime_shutdown(ane);
		break;
	case AURORA_ANE_RUN:
		if (copy_from_user(&run, arg, sizeof(run))) {
			ret = -EFAULT;
			break;
		}
		ret = ane_user_request_valid(&run);
		if (ret)
			break;
		if (ane->runtime_state != AURORA_ANE_STATE_LOADED) {
			ret = ane->runtime_state == AURORA_ANE_STATE_FAULT ? -EIO : -ESHUTDOWN;
			break;
		}
		if (ane->completed_jobs >= AURORA_ANE_MAX_JOBS) {
			ret = -EOVERFLOW;
			break;
		}
		if (!access_ok(u64_to_user_ptr(run.output), run.output_bytes) || !access_ok(arg, sizeof(run))) {
			ret = -EFAULT;
			break;
		}
		input = memdup_user(u64_to_user_ptr(run.input), run.input_bytes);
		if (IS_ERR(input)) {
			ret = PTR_ERR(input);
			break;
		}
		ret = 0;
		for (i = 0; i < run.input_bytes / 2; i++)
			if ((get_unaligned_le16(input + i * 2) & 0x7c00) == 0x7c00) {
				ret = -EDOM;
				break;
			}
		start = ktime_get_ns();
		if (!ret)
			ret = ane_runtime_execute(ane, input);
		kfree(input);
		if (ret)
			break;
		run.elapsed_ns = ktime_get_ns() - start;
		run.sequence = ane->completed_jobs;
		if (copy_to_user(u64_to_user_ptr(run.output), ane->runtime_output, run.output_bytes) ||
		    copy_to_user(arg, &run, sizeof(run)))
			ret = -EFAULT;
		break;
	default:
		ret = -ENOTTY;
	}
	mutex_unlock(&ane->mutex);
	return ret;
}

static int ane_user_open(struct inode *inode, struct file *file)
{
	struct ane_fw_test *ane = container_of(file->private_data, struct ane_fw_test, misc);
	int ret;

	if (!capable(CAP_SYS_RAWIO))
		return -EPERM;
	ret = mutex_lock_interruptible(&ane->mutex);
	if (ret)
		return ret;
	if (ane->user_open || (ane->case_ran && !ane->session_mode)) {
		ret = -EBUSY;
	} else {
		ane->user_open = true;
		file->private_data = ane;
		ret = nonseekable_open(inode, file);
	}
	mutex_unlock(&ane->mutex);
	return ret;
}

static int ane_user_release(struct inode *inode, struct file *file)
{
	struct ane_fw_test *ane = file->private_data;

	mutex_lock(&ane->mutex);
	if (ane->runtime_state == AURORA_ANE_STATE_LOADED)
		ane_runtime_shutdown(ane);
	ane_runtime_drop_cpu(ane);
	ane->user_open = false;
	mutex_unlock(&ane->mutex);
	return 0;
}

static const struct file_operations ane_user_fops = {
	.owner = THIS_MODULE,
	.open = ane_user_open,
	.release = ane_user_release,
	.unlocked_ioctl = ane_user_ioctl,
	.compat_ioctl = compat_ptr_ioctl,
};
#endif

static ssize_t status_show(struct device *dev, struct device_attribute *attr, char *buf)
{
	struct ane_fw_test *ane = dev_get_drvdata(dev);
	ssize_t ret;

	mutex_lock(&ane->mutex);
	ret = sysfs_emit(buf, "state=%s error=%d started=%u allocations=%u shared_bytes=%u samples_passed=%u shutdown_reply=%u\n",
			 ane->state, ane->last_error, ane->started,
			 ane->allocations, ane->shared_bytes, ane->samples_passed, ane->shutdown_reply);
	mutex_unlock(&ane->mutex);
	return ret;
}
static DEVICE_ATTR_RO(status);

static struct attribute *ane_fw_attrs[] = {
	&dev_attr_start_bootstrap.attr, &dev_attr_status.attr,
#ifdef ANE_HAVE_CASE
	&dev_attr_test_compute.attr,
#endif
	NULL,
};
ATTRIBUTE_GROUPS(ane_fw);

static int ane_pm_notify(struct notifier_block *notifier, unsigned long event, void *unused)
{
	struct ane_fw_test *ane = container_of(notifier, struct ane_fw_test, pm_notifier);

	if ((event == PM_SUSPEND_PREPARE || event == PM_HIBERNATION_PREPARE) &&
	    READ_ONCE(ane->started))
		return NOTIFY_BAD;
	return NOTIFY_DONE;
}

static void ane_fw_cleanup(void *data)
{
	struct ane_fw_test *ane = data;
	unsigned int i;

	if (WARN_ON(ane->started))
		return;
	if (ane->misc_registered)
		misc_deregister(&ane->misc);
	if (ane->notifier_registered)
		unregister_pm_notifier(&ane->pm_notifier);
	ane_release_memory(ane);
	if (ane->memory)
		root_device_unregister(ane->memory);
	for (i = 0; i < ARRAY_SIZE(ane->dart); i++)
		if (ane->dart[i]) {
			pm_runtime_put_sync(ane->dart[i]);
			put_device(ane->dart[i]);
		}
	apple_ane_bringup_put_engine(ane->power);
}

static void __iomem *ane_map_resource(struct platform_device *pdev, const char *name,
				    phys_addr_t base, u32 size)
{
	struct resource *res = platform_get_resource_byname(pdev, IORESOURCE_MEM, name);

	if (!res || res->start != base || resource_size(res) != size)
		return ERR_PTR(-EINVAL);
	return devm_ioremap_resource(&pdev->dev, res);
}

static int ane_fw_probe(struct platform_device *pdev)
{
	struct device *dev = &pdev->dev;
	struct of_phandle_args args;
	struct platform_device *dart;
	struct ane_fw_test *ane;
	struct resource resource;
	unsigned int i;
	int ret;

	if (!of_machine_is_compatible("apple,j700") ||
	    !of_machine_is_compatible("apple,t8140") || PAGE_SIZE != ANE_GRANULE ||
	    of_count_phandle_with_args(dev->of_node, "iommus", "#iommu-cells") != 3)
		return -ENODEV;
	ane = devm_kzalloc(dev, sizeof(*ane), GFP_KERNEL);
	if (!ane)
		return -ENOMEM;
	ane->dev = dev;
	ane->state = "ready";
	ane->next_iova = ANE_IOVA + ANE_ARENA_SIZE;
	INIT_LIST_HEAD(&ane->buffers);
	mutex_init(&ane->mutex);
	platform_set_drvdata(pdev, ane);
	ret = devm_add_action_or_reset(dev, ane_fw_cleanup, ane);
	if (ret)
		return ret;
	ret = apple_ane_bringup_get_engine(&ane->power);
	if (ret)
		return dev_err_probe(dev, ret, "expected powered J700 sequencer\n");
	for (i = 0; i < ARRAY_SIZE(ane->dart); i++) {
		ret = of_parse_phandle_with_args(dev->of_node, "iommus", "#iommu-cells", i, &args);
		if (ret)
			return ret;
		ret = of_address_to_resource(args.np, 0, &resource);
		if (!ret && (args.args_count != 1 || args.args[0] ||
			     resource.start != ane_trad_bases[i] ||
			     resource_size(&resource) != (i ? 0x20000 : 0xc000) ||
			     !of_device_is_compatible(args.np, "apple,t8110-dart")))
			ret = -EINVAL;
		dart = ret ? NULL : of_find_device_by_node(args.np);
		of_node_put(args.np);
		if (ret)
			return ret;
		if (!dart)
			return -EPROBE_DEFER;
		ret = pm_runtime_resume_and_get(&dart->dev);
		if (ret < 0) {
			put_device(&dart->dev);
			return ret;
		}
		ane->dart[i] = &dart->dev;
		/* Read-only observer of the provider's status/protection registers.
		 * The DART driver retains exclusive ownership of translation writes.
		 */
		ane->trad[i] = devm_ioremap(dev, resource.start, 0x400);
		if (!ane->trad[i])
			return -ENOMEM;
	}
#define ANE_MAP(field, address, size) do { \
	ane->field = ane_map_resource(pdev, #field, address, size); \
	if (IS_ERR(ane->field)) \
		return PTR_ERR(ane->field); \
} while (0)
	ANE_MAP(cpu, 0x401600040ULL, 0x10);
	ANE_MAP(vector, 0x401050000ULL, 8);
	ANE_MAP(scratch, 0x401880020ULL, 0x20);
	ANE_MAP(doorbell, 0x401884000ULL, 4);
	ANE_MAP(irq, 0x40188c000ULL, 4);
	ANE_MAP(ack, 0x401890000ULL, 4);
	ANE_MAP(dapf, 0x401810000ULL, 0x100);
#undef ANE_MAP
	ane->memory = root_device_register("j700-ane-test-memory");
	if (IS_ERR(ane->memory)) {
		ret = PTR_ERR(ane->memory);
		ane->memory = NULL;
		return ret;
	}
	ret = dma_coerce_mask_and_coherent(ane->memory, DMA_BIT_MASK(42));
	if (ret)
		return ret;
	ane->pm_notifier.notifier_call = ane_pm_notify;
	ret = register_pm_notifier(&ane->pm_notifier);
	if (ret)
		return ret;
	ane->notifier_registered = true;
#ifdef ANE_CASE_ANEMLL
	ane->misc = (struct miscdevice) {
		.minor = MISC_DYNAMIC_MINOR, .name = "aurora-ane0",
		.fops = &ane_user_fops, .parent = dev, .mode = 0600,
	};
	ret = misc_register(&ane->misc);
	if (ret)
		return ret;
	ane->misc_registered = true;
#endif
	dev_info(dev, "J700/25G83 firmware transport test ready; firmware has not started\n");
	return 0;
}

static const struct of_device_id ane_fw_match[] = {
	{ .compatible = "apple,t8140-ane-fw-test" }, {},
};
MODULE_DEVICE_TABLE(of, ane_fw_match);

static struct platform_driver ane_fw_driver = {
	.probe = ane_fw_probe,
	/* platform_dma_configure() must not claim the default DMA domain: the
	 * test explicitly claims an unmanaged domain before mapping firmware.
	 */
	.driver_managed_dma = true,
	.driver = {
		.name = "apple-ane-fw-test",
		.of_match_table = ane_fw_match,
		.dev_groups = ane_fw_groups,
		.suppress_bind_attrs = true,
	},
};
module_platform_driver(ane_fw_driver);
MODULE_FIRMWARE(ANE_FW_NAME);
MODULE_DESCRIPTION("J700 RAM-only pinned firmware and Linux IOMMU transport experiment");
MODULE_LICENSE("GPL");
