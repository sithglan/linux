// SPDX-License-Identifier: GPL-2.0
/*
 * Apple APCIe peripheral-I/O DMA transport.
 *
 * T8140 routes downstream PCI configuration and selected endpoint-memory
 * reads through a PIODMA engine.  The engine consumes a 16-byte generic
 * packet through DART stream 17 and returns completion through a second DMA
 * packet.  The register and packet contract below is measured from the exact
 * J700 25G83 AppleT6020PCIePIODMA implementation.
 */

#include <linux/bitfield.h>
#include <linux/dma-mapping.h>
#include <linux/iommu.h>
#include <linux/iopoll.h>
#include <linux/io.h>
#include <linux/interrupt.h>
#include <linux/kernel.h>
#include <linux/mm.h>
#include <linux/module.h>
#include <linux/of.h>
#include <linux/of_device.h>
#include <linux/of_dma.h>
#include <linux/of_platform.h>
#include <linux/platform_device.h>
#include <linux/sizes.h>
#include <linux/slab.h>
#include <linux/spinlock.h>

#include "../pci.h"
#include "pcie-apple-piodma.h"

static bool skip_hw_init;
module_param(skip_hw_init, bool, 0644);
MODULE_PARM_DESC(skip_hw_init,
		 "attach the PIODMA SID17 domain without reprogramming the engine");

#define APPLE_PIODMA_APERTURES		3
#define APPLE_PIODMA_PACKET_BYTES	0x100
#define APPLE_PIODMA_TIMEOUT_US		50000
#define APPLE_PIODMA_DATA_OFFSET	0x1610
#define APPLE_PIODMA_SOURCE_OFFSET	0x1b90
#define APPLE_PIODMA_COMPLETION_OFFSET	0x3b10
#define APPLE_PIODMA_DATA_IOVA		0x075f8000ULL
#define APPLE_PIODMA_SOURCE_IOVA	0x07600000ULL
#define APPLE_PIODMA_COMPLETION_IOVA	0x07608000ULL
#define APPLE_PIODMA_ENGINE_IOVA_HIGH	BIT_ULL(40)

#define PIODMA_CFG			0x000
#define  PIODMA_CFG_DISABLE		BIT(0)
#define PIODMA_IRQ_STATUS		0x004
#define  PIODMA_IRQ_COMMAND_COMPLETE	(BIT(6) | BIT(0))
#define PIODMA_IRQ_ENABLE		0x008
#define PIODMA_FIFO_STATUS		0x00c
#define  PIODMA_FIFO_FREE		GENMASK(5, 0)
#define PIODMA_TUNE_10			0x010
#define PIODMA_TUNE_14			0x014
#define PIODMA_TUNE_18			0x018
#define PIODMA_TUNE_1C			0x01c
#define PIODMA_TUNE_20			0x020
#define PIODMA_REG_24			0x024
#define PIODMA_REG_28			0x028
#define PIODMA_REG_2C			0x02c
#define PIODMA_REG_30			0x030
#define PIODMA_REG_34			0x034
#define PIODMA_REG_38			0x038
#define PIODMA_REG_3C			0x03c
#define PIODMA_REG_40			0x040
#define PIODMA_DST_LO			0x044
#define PIODMA_DST_HI			0x048
#define PIODMA_SRC_LO			0x04c
#define PIODMA_SRC_HI			0x050
#define PIODMA_COMMAND			0x054
#define  PIODMA_COMMAND_START		BIT(0)
#define  PIODMA_COMMAND_GENERIC		BIT(4)
#define  PIODMA_COMMAND_SIZE		GENMASK(29, 8)
#define PIODMA_TAG_SUBMIT		0x0d8
#define PIODMA_TAG_COMPLETE		0x0dc
#define PIODMA_REG_F0			0x0f0
#define PIODMA_REG_F4			0x0f4
#define PIODMA_REG_F8			0x0f8
#define PIODMA_REG_FC			0x0fc
#define PIODMA_REG_100			0x100
#define PIODMA_REG_104			0x104
#define PIODMA_REG_108			0x108
#define PIODMA_REG_10C			0x10c

#define PIODMA_PACKET_VALID		0xc007
#define PIODMA_PACKET_SRC_APERTURE	GENMASK(20, 18)
#define PIODMA_PACKET_DST_APERTURE	GENMASK(23, 21)
#define PIODMA_PACKET_SRC_LOW		GENMASK(31, 24)
#define PIODMA_PACKET_SRC_MID		GENMASK(27, 0)
#define PIODMA_PACKET_DST_LOW		GENMASK(31, 28)

struct apple_piodma_packet {
	__le32 control;
	__le32 address_mid;
	__le32 destination;
	__le32 length;
};

struct apple_pcie_piodma {
	struct device *dev;
	void __iomem *engine;
	void __iomem *irq_aggregator;
	void __iomem *fabric;
	u64 aperture_base[APPLE_PIODMA_APERTURES];
	u64 aperture_size[APPLE_PIODMA_APERTURES];
	raw_spinlock_t lock;
	u8 next_tag;
	atomic_t irq_epoch;
	atomic_t irq_ack_epoch;
	u8 completed_tag;
	u32 completed_status;
	u64 irq_count;

	void *source_page;
	dma_addr_t source_map_dma;
	dma_addr_t source_domain_iova;
	dma_addr_t source_dma;
	bool source_mapped;
	void *completion_page;
	dma_addr_t completion_map_dma;
	dma_addr_t completion_domain_iova;
	dma_addr_t completion_dma;
	bool completion_mapped;
	void *data_page;
	dma_addr_t data_map_dma;
	dma_addr_t data_domain_iova;
	dma_addr_t data_dma;
	struct iommu_domain *domain;
	bool domain_attached;
	bool source_domain_mapped;
	bool completion_domain_mapped;
	bool data_domain_mapped;

	u64 commands;
	u64 reads;
	u64 writes;
	bool initialized;
	bool faulted;
};

static void apple_piodma_hw_init(struct apple_pcie_piodma *piodma);

static void apple_piodma_rmw(void __iomem *base, u32 offset,
			     u32 mask, u32 value)
{
	u32 reg = readl_relaxed(base + offset);
	u32 next = (reg & ~mask) | (value & mask);

	/* AppleT6020PCIePIODMA writes a tunable only when its masked value
	 * changes. On Run274, 0x18/0x1c/0x20 are read but deliberately not
	 * written because iBoot already supplied the requested values.
	 */
	if (next != reg)
		writel_relaxed(next, base + offset);
}

static void apple_piodma_build_packet(struct apple_piodma_packet *packet,
				      u64 source, u8 source_aperture,
				      u64 destination, u8 destination_aperture,
				      size_t length)
{
	u32 control;

	control = PIODMA_PACKET_VALID |
		FIELD_PREP(PIODMA_PACKET_SRC_APERTURE, source_aperture) |
		FIELD_PREP(PIODMA_PACKET_DST_APERTURE, destination_aperture) |
		FIELD_PREP(PIODMA_PACKET_SRC_LOW, source & 0xff);
	packet->control = cpu_to_le32(control);
	packet->address_mid = cpu_to_le32(
		FIELD_PREP(PIODMA_PACKET_SRC_MID, (source >> 8) & GENMASK_ULL(27, 0)) |
		FIELD_PREP(PIODMA_PACKET_DST_LOW, destination & 0xf));
	packet->destination = cpu_to_le32(destination >> 4);
	packet->length = cpu_to_le32(length);
}

static int apple_piodma_packet_selftest(void)
{
	struct apple_piodma_packet packet;

	/* Run275: downstream ECAM read, 01:00.0 offset 0x84, four bytes. */
	apple_piodma_build_packet(&packet, 0x1cb0100084ULL, 1,
				  0x075f9610, 0, 4);
	if (le32_to_cpu(packet.control) != 0x8404c007 ||
	    le32_to_cpu(packet.address_mid) != 0x0cb01000 ||
	    le32_to_cpu(packet.destination) != 0x0075f961 ||
	    le32_to_cpu(packet.length) != 4)
		return -EINVAL;

	return 0;
}

static int apple_piodma_aperture(struct apple_pcie_piodma *piodma,
				 phys_addr_t address)
{
	int i;

	for (i = 0; i < APPLE_PIODMA_APERTURES; i++) {
		if (address >= piodma->aperture_base[i] &&
		    address - piodma->aperture_base[i] < piodma->aperture_size[i])
			return i + 1;
	}

	return -ERANGE;
}

static int apple_piodma_wait_fifo(struct apple_pcie_piodma *piodma)
{
	u32 status;

	return readl_poll_timeout_atomic(piodma->engine + PIODMA_FIFO_STATUS,
					 status, FIELD_GET(PIODMA_FIFO_FREE, status),
					 1, APPLE_PIODMA_TIMEOUT_US);
}

static u32 apple_piodma_ack(struct apple_pcie_piodma *piodma)
{
	u32 fabric, status;

	fabric = readl(piodma->fabric);
	writel(fabric, piodma->fabric);
	status = readl(piodma->engine + PIODMA_IRQ_STATUS);
	writel(status, piodma->engine + PIODMA_IRQ_STATUS);

	return status;
}

static irqreturn_t apple_piodma_irq(int irq, void *data)
{
	struct apple_pcie_piodma *piodma = data;
	u32 status;

	status = apple_piodma_ack(piodma);
	if (status & PIODMA_IRQ_COMMAND_COMPLETE) {
		WRITE_ONCE(piodma->completed_status, status);
		WRITE_ONCE(piodma->completed_tag,
			   readl(piodma->engine + PIODMA_TAG_COMPLETE) &
				GENMASK(7, 0));
		smp_wmb();
		atomic_inc(&piodma->irq_epoch);
	}
	piodma->irq_count++;
	smp_wmb();
	atomic_inc(&piodma->irq_ack_epoch);

	return IRQ_HANDLED;
}

static int apple_piodma_submit(struct apple_pcie_piodma *piodma,
			       phys_addr_t address, size_t size,
			       u32 *value, bool write, bool pci_config)
{
	struct apple_piodma_packet *packet =
		piodma->source_page + APPLE_PIODMA_SOURCE_OFFSET;
	void *completion =
		piodma->completion_page + APPLE_PIODMA_COMPLETION_OFFSET;
	void *data = piodma->data_page + APPLE_PIODMA_DATA_OFFSET;
	u64 source, destination;
	u32 ack_epoch, command, epoch, status;
	u32 fail_command = 0, fail_fifo = 0, fail_irq = 0;
	u8 source_aperture, destination_aperture, tag = 0;
	u8 fail_complete = 0;
	bool first_success = false;
	int aperture, ret;

	if (!value || size != 4)
		return -EINVAL;

	aperture = apple_piodma_aperture(piodma, address);
	if (aperture < 0)
		return aperture;
	if (upper_32_bits(piodma->source_dma) & ~GENMASK(9, 0) ||
	    upper_32_bits(piodma->completion_dma) & ~GENMASK(9, 0) ||
	    piodma->data_dma >= BIT_ULL(36))
		return -ERANGE;

	raw_spin_lock(&piodma->lock);
	if (piodma->faulted) {
		ret = -EIO;
		goto out_unlock;
	}
	if (!piodma->initialized) {
		/* Link setup runs after the supplier probe but before config
		 * scanning. Initialize once here, after that setup. Never reset
		 * or replay a command on an engine which has already failed.
		 */
		dev_info(piodma->dev,
			 "J700_APCIE_PIODMA_BEFORE_FIRST_COMMAND: cfg=%08x irq_enable=%08x aperture1_hi=%08x\n",
			 readl(piodma->engine + PIODMA_CFG),
			 readl(piodma->engine + PIODMA_IRQ_ENABLE),
			 readl(piodma->engine + PIODMA_REG_F4));
		if (!skip_hw_init)
			apple_piodma_hw_init(piodma);
		piodma->initialized = true;
	}
	if (readl(piodma->engine + PIODMA_CFG) != 0 ||
	    readl(piodma->engine + PIODMA_IRQ_ENABLE) != 0x3ff ||
	    readl(piodma->engine + PIODMA_REG_F4) != 1) {
		dev_err(piodma->dev,
			"PIODMA configuration lost before submission; DMA blocked\n");
		ret = -EIO;
		goto out_unlock;
	}

	memset(packet, 0, APPLE_PIODMA_PACKET_BYTES);
	memset(completion, 0, APPLE_PIODMA_PACKET_BYTES);
	if (write)
		memcpy(data, value, size);
	else
		memset(data, 0xff, size);

	if (write) {
		source = piodma->data_dma;
		destination = address;
		source_aperture = 0;
		destination_aperture = aperture;
	} else {
		source = address;
		destination = piodma->data_dma;
		source_aperture = aperture;
		destination_aperture = 0;
	}
	apple_piodma_build_packet(packet, source, source_aperture,
				  destination, destination_aperture, size);
	dma_sync_single_for_device(piodma->dev, piodma->source_map_dma,
				   PAGE_SIZE, DMA_TO_DEVICE);
	dma_sync_single_for_device(piodma->dev, piodma->completion_map_dma,
				   PAGE_SIZE, DMA_FROM_DEVICE);
	dma_wmb();

	ret = apple_piodma_wait_fifo(piodma);
	if (ret)
		goto out_unlock;

	tag = piodma->next_tag++;
	writel_relaxed(tag, piodma->engine + PIODMA_TAG_SUBMIT);
	writel_relaxed(upper_32_bits(piodma->source_dma) & GENMASK(9, 0),
		       piodma->engine + PIODMA_SRC_HI);
	writel_relaxed(lower_32_bits(piodma->source_dma),
		       piodma->engine + PIODMA_SRC_LO);
	writel_relaxed(upper_32_bits(piodma->completion_dma) & GENMASK(9, 0),
		       piodma->engine + PIODMA_DST_HI);
	writel_relaxed(lower_32_bits(piodma->completion_dma),
		       piodma->engine + PIODMA_DST_LO);
	command = PIODMA_COMMAND_GENERIC |
		  FIELD_PREP(PIODMA_COMMAND_SIZE, size) |
		  PIODMA_COMMAND_START;
	epoch = atomic_read(&piodma->irq_epoch);
	ack_epoch = atomic_read(&piodma->irq_ack_epoch);
	writel(command, piodma->engine + PIODMA_COMMAND);

	if (pci_config)
		raw_spin_unlock_irq(&pci_lock);
	ret = read_poll_timeout_atomic(atomic_read, status, status != epoch,
				       1, APPLE_PIODMA_TIMEOUT_US, false,
				       &piodma->irq_epoch);
	if (pci_config)
		raw_spin_lock_irq(&pci_lock);
	if (ret)
		goto out_unlock;
	smp_rmb();
	if (READ_ONCE(piodma->completed_tag) != tag) {
		ret = -EIO;
		goto out_unlock;
	}

	/* Run275 records a second, status-zero IRQ callback before reuse. */
	ret = read_poll_timeout_atomic(atomic_read, status,
				       (u32)(status - ack_epoch) >= 2,
				       1, APPLE_PIODMA_TIMEOUT_US, false,
				       &piodma->irq_ack_epoch);
	if (ret)
		goto out_unlock;
	dma_rmb();
	dma_sync_single_for_cpu(piodma->dev, piodma->source_map_dma,
				PAGE_SIZE, DMA_TO_DEVICE);
	dma_sync_single_for_cpu(piodma->dev, piodma->completion_map_dma,
				PAGE_SIZE, DMA_FROM_DEVICE);
	if (!write)
		memcpy(value, data, size);
	first_success = !piodma->commands;
	piodma->commands++;
	if (write)
		piodma->writes++;
	else
		piodma->reads++;

out_unlock:
	if (ret) {
		piodma->faulted = true;
		fail_command = readl_relaxed(piodma->engine + PIODMA_COMMAND);
		fail_fifo = readl_relaxed(piodma->engine + PIODMA_FIFO_STATUS);
		fail_irq = readl_relaxed(piodma->engine + PIODMA_IRQ_STATUS);
		fail_complete = readl_relaxed(piodma->engine + PIODMA_TAG_COMPLETE) &
				GENMASK(7, 0);
	}
	raw_spin_unlock(&piodma->lock);

	if (ret)
		dev_err_ratelimited(piodma->dev,
			"command failed: address=%pa size=%zu write=%u ret=%d cfg=%08x fifo=%08x irq=%08x tag=%02x/%02x\n",
			&address, size, write, ret, fail_command, fail_fifo,
			fail_irq, tag, fail_complete);
	else if (first_success)
		dev_info(piodma->dev,
			 "J700_APCIE_PIODMA_FIRST_COMMAND_PASS: address=%pa size=%zu write=%u value=%08x tag=%02x\n",
			 &address, size, write, *value, tag);

	return ret;
}

static void apple_piodma_hw_init(struct apple_pcie_piodma *piodma)
{
	writel_relaxed(0x3ff, piodma->engine + PIODMA_IRQ_ENABLE);
	writel_relaxed(0, piodma->fabric + 0x4);
	apple_piodma_rmw(piodma->engine, PIODMA_CFG, BIT(1), 0);
	apple_piodma_rmw(piodma->engine, PIODMA_TUNE_10, GENMASK(28, 0), 0x1fbf2);
	apple_piodma_rmw(piodma->engine, PIODMA_TUNE_14, GENMASK(28, 0), 0x1fbf2);
	apple_piodma_rmw(piodma->engine, PIODMA_TUNE_18, GENMASK(28, 0), 0x1fbf3);
	apple_piodma_rmw(piodma->engine, PIODMA_TUNE_1C, GENMASK(18, 0), 0x1fbf0);
	apple_piodma_rmw(piodma->engine, PIODMA_TUNE_20, GENMASK(18, 0), 0x1fbf0);

	/* Pinned J700 25G76 enableGated() clears CFG bit 0; disableGated()
	 * sets it. This is a stop bit, not an enable bit. Clear it before
	 * programming the aperture banks and leave it clear for submissions.
	 */
	writel_relaxed(readl_relaxed(piodma->engine + PIODMA_CFG) & ~PIODMA_CFG_DISABLE,
		       piodma->engine + PIODMA_CFG);
	writel_relaxed(0, piodma->engine + PIODMA_REG_24);
	writel_relaxed(0, piodma->engine + PIODMA_REG_F0);
	writel_relaxed(0, piodma->engine + PIODMA_REG_28);
	writel_relaxed(1, piodma->engine + PIODMA_REG_F4);
	writel_relaxed(0, piodma->engine + PIODMA_REG_2C);
	writel_relaxed(0, piodma->engine + PIODMA_REG_F8);
	writel_relaxed(0, piodma->engine + PIODMA_REG_30);
	writel_relaxed(0, piodma->engine + PIODMA_REG_FC);
	writel_relaxed(0, piodma->engine + PIODMA_REG_34);
	writel_relaxed(0, piodma->engine + PIODMA_REG_100);
	writel_relaxed(0, piodma->engine + PIODMA_REG_38);
	writel_relaxed(0, piodma->engine + PIODMA_REG_104);
	writel_relaxed(0, piodma->engine + PIODMA_REG_3C);
	writel_relaxed(0, piodma->engine + PIODMA_REG_108);
	writel_relaxed(0, piodma->engine + PIODMA_REG_40);
	writel_relaxed(0, piodma->engine + PIODMA_REG_10C);
	dev_info(piodma->dev,
		 "J700_APCIE_PIODMA_APERTURES: cfg=%08x bank0=%08x/%08x bank1=%08x/%08x bank2=%08x/%08x bank3=%08x/%08x\n",
		 readl(piodma->engine + PIODMA_CFG),
		 readl(piodma->engine + PIODMA_REG_24),
		 readl(piodma->engine + PIODMA_REG_F0),
		 readl(piodma->engine + PIODMA_REG_28),
		 readl(piodma->engine + PIODMA_REG_F4),
		 readl(piodma->engine + PIODMA_REG_2C),
		 readl(piodma->engine + PIODMA_REG_F8),
		 readl(piodma->engine + PIODMA_REG_30),
		 readl(piodma->engine + PIODMA_REG_FC));
	wmb();
	dev_info(piodma->dev,
		 "J700_APCIE_PIODMA_HW_INIT_PASS: cfg=0x%08x stop=0\n",
		 readl_relaxed(piodma->engine + PIODMA_CFG));
}

static int apple_piodma_map_buffers(struct apple_pcie_piodma *piodma)
{
	piodma->source_page = (void *)get_zeroed_page(GFP_KERNEL);
	if (!piodma->source_page)
		return -ENOMEM;
	piodma->completion_page = piodma->source_page;

	piodma->source_map_dma = dma_map_single(piodma->dev,
						piodma->source_page,
						PAGE_SIZE, DMA_TO_DEVICE);
	if (dma_mapping_error(piodma->dev, piodma->source_map_dma))
		return -ENOMEM;
	piodma->source_mapped = true;
	piodma->source_domain_iova = APPLE_PIODMA_SOURCE_IOVA;
	piodma->source_dma = APPLE_PIODMA_ENGINE_IOVA_HIGH |
			      (piodma->source_domain_iova +
			       APPLE_PIODMA_SOURCE_OFFSET);
	piodma->completion_map_dma = dma_map_single(piodma->dev,
						    piodma->completion_page,
						    PAGE_SIZE, DMA_FROM_DEVICE);
	if (dma_mapping_error(piodma->dev, piodma->completion_map_dma))
		return -ENOMEM;
	piodma->completion_mapped = true;
	piodma->completion_domain_iova = APPLE_PIODMA_COMPLETION_IOVA;
	piodma->completion_dma = APPLE_PIODMA_ENGINE_IOVA_HIGH |
				  (piodma->completion_domain_iova +
				   APPLE_PIODMA_COMPLETION_OFFSET);

	piodma->data_page = dma_alloc_coherent(piodma->dev, PAGE_SIZE,
						&piodma->data_map_dma, GFP_KERNEL);
	if (!piodma->data_page)
		return -ENOMEM;
	piodma->data_domain_iova = APPLE_PIODMA_DATA_IOVA;
	piodma->data_dma = piodma->data_domain_iova + APPLE_PIODMA_DATA_OFFSET;

	/* dma_map_single() initially transfers ownership to the device. */
	dma_sync_single_for_cpu(piodma->dev, piodma->source_map_dma,
				PAGE_SIZE, DMA_TO_DEVICE);
	dma_sync_single_for_cpu(piodma->dev, piodma->completion_map_dma,
				PAGE_SIZE, DMA_FROM_DEVICE);

	return 0;
}

static void apple_piodma_unmap_buffers(struct apple_pcie_piodma *piodma)
{
	if (piodma->data_page)
		dma_free_coherent(piodma->dev, PAGE_SIZE, piodma->data_page,
				  piodma->data_map_dma);
	if (piodma->completion_mapped)
		dma_unmap_single(piodma->dev, piodma->completion_map_dma,
				 PAGE_SIZE, DMA_FROM_DEVICE);
	if (piodma->source_mapped)
		dma_unmap_single(piodma->dev, piodma->source_map_dma,
				 PAGE_SIZE, DMA_TO_DEVICE);
	if (piodma->source_page)
		free_page((unsigned long)piodma->source_page);
}

static void apple_piodma_unmap_buffers_action(void *data)
{
	apple_piodma_unmap_buffers(data);
}

static int apple_piodma_map_domain_page(struct apple_pcie_piodma *piodma,
					dma_addr_t dma_alias,
					dma_addr_t domain_iova, int prot,
					bool *mapped)
{
	struct iommu_domain *dma_domain;
	phys_addr_t phys;
	int ret;

	dma_domain = iommu_get_domain_for_dev(piodma->dev);
	if (!dma_domain)
		return -ENODEV;
	phys = iommu_iova_to_phys(dma_domain, dma_alias);
	if (!phys)
		return -EFAULT;

	ret = iommu_map(piodma->domain, domain_iova, phys, PAGE_SIZE, prot,
			GFP_KERNEL);
	if (!ret)
		*mapped = true;
	return ret;
}

static void apple_piodma_unmap_domain(struct apple_pcie_piodma *piodma)
{
	if (!piodma->domain)
		return;
	if (piodma->data_domain_mapped)
		iommu_unmap(piodma->domain, piodma->data_domain_iova, PAGE_SIZE);
	if (piodma->completion_domain_mapped)
		iommu_unmap(piodma->domain, piodma->completion_domain_iova,
			    PAGE_SIZE);
	if (piodma->source_domain_mapped)
		iommu_unmap(piodma->domain, piodma->source_domain_iova, PAGE_SIZE);
	if (piodma->domain_attached)
		iommu_detach_device(piodma->domain, piodma->dev);
	iommu_domain_free(piodma->domain);
	piodma->domain = NULL;
}

static void apple_piodma_unmap_domain_action(void *data)
{
	apple_piodma_unmap_domain(data);
}

static int apple_piodma_setup_unmanaged_domain(struct apple_pcie_piodma *piodma)
{
	int ret;

	/*
	 * PIODMA is a memory-to-MMIO engine, not a normal DMA consumer.  Keep
	 * the DMA-API mappings above as cache-maintenance aliases, but expose
	 * the pages to SID17 through a dedicated unmanaged domain.  This follows
	 * the established Apple DCP PIODMA arrangement and prevents the generic
	 * DMA domain from owning the engine's translation context.
	 */
	piodma->domain = iommu_paging_domain_alloc(piodma->dev);
	if (IS_ERR(piodma->domain)) {
		ret = PTR_ERR(piodma->domain);
		piodma->domain = NULL;
		return ret;
	}

	ret = apple_piodma_map_domain_page(piodma, piodma->source_map_dma,
					    piodma->source_domain_iova,
					    IOMMU_READ | IOMMU_CACHE,
					    &piodma->source_domain_mapped);
	if (ret)
		goto err;
	ret = apple_piodma_map_domain_page(piodma, piodma->completion_map_dma,
					    piodma->completion_domain_iova,
					    IOMMU_WRITE | IOMMU_CACHE,
					    &piodma->completion_domain_mapped);
	if (ret)
		goto err;
	ret = apple_piodma_map_domain_page(piodma, piodma->data_map_dma,
					    piodma->data_domain_iova,
					    IOMMU_READ | IOMMU_WRITE | IOMMU_CACHE,
					    &piodma->data_domain_mapped);
	if (ret)
		goto err;

	ret = iommu_attach_device(piodma->domain, piodma->dev);
	if (ret)
		goto err;
	piodma->domain_attached = true;
	return 0;

err:
	apple_piodma_unmap_domain(piodma);
	return ret;
}

struct apple_pcie_piodma *
apple_pcie_piodma_create(struct device *parent, struct device_node *node)
{
	struct apple_pcie_piodma *piodma;
	struct platform_device *pdev;
	struct device_link *link;

	if (!node || !of_device_is_compatible(node, "apple,t8140-pcie-piodma"))
		return ERR_PTR(-ENODEV);
	pdev = of_find_device_by_node(node);
	if (!pdev)
		return ERR_PTR(-EPROBE_DEFER);
	piodma = platform_get_drvdata(pdev);
	if (!piodma) {
		put_device(&pdev->dev);
		return ERR_PTR(-EPROBE_DEFER);
	}
	link = device_link_add(parent, &pdev->dev,
			       DL_FLAG_AUTOREMOVE_CONSUMER | DL_FLAG_PM_RUNTIME);
	put_device(&pdev->dev);
	if (!link)
		return ERR_PTR(-ENOMEM);

	return piodma;
}
EXPORT_SYMBOL_GPL(apple_pcie_piodma_create);

static int apple_piodma_probe(struct platform_device *pdev)
{
	struct apple_pcie_piodma *piodma;
	struct device *dev = &pdev->dev;
	struct device_node *node = dev->of_node;
	int irq, ret;

	if (PAGE_SIZE != SZ_16K)
		return dev_err_probe(dev, -EINVAL,
				     "T8140 PIODMA requires 16 KiB pages\n");
	ret = apple_piodma_packet_selftest();
	if (ret)
		return dev_err_probe(dev, ret, "packet self-test failed\n");

	piodma = devm_kzalloc(dev, sizeof(*piodma), GFP_KERNEL);
	if (!piodma)
		return -ENOMEM;
	piodma->dev = dev;

	/*
	 * The engine's source/completion pointer registers accept 42-bit IOVAs,
	 * but the generic packet carries its memory-side address as a 32-bit
	 * value shifted by four.  Run275 used low IOVAs for all three objects;
	 * constraining the supplier to 36 bits makes that packet ABI invariant.
	 */
	ret = dma_set_mask_and_coherent(dev, DMA_BIT_MASK(36));
	if (ret)
		return dev_err_probe(dev, ret, "failed to set DMA mask\n");

	piodma->engine = devm_platform_ioremap_resource(pdev, 0);
	if (IS_ERR(piodma->engine))
		return PTR_ERR(piodma->engine);
	piodma->irq_aggregator = devm_platform_ioremap_resource(pdev, 1);
	if (IS_ERR(piodma->irq_aggregator))
		return PTR_ERR(piodma->irq_aggregator);
	piodma->fabric = devm_platform_ioremap_resource(pdev, 2);
	if (IS_ERR(piodma->fabric))
		return PTR_ERR(piodma->fabric);

	ret = of_property_read_u64_array(node, "apple,piodma-base-addresses",
					 piodma->aperture_base,
					 APPLE_PIODMA_APERTURES);
	if (ret)
		return dev_err_probe(dev, ret, "missing aperture bases\n");
	ret = of_property_read_u64_array(node, "apple,piodma-aperture-sizes",
					 piodma->aperture_size,
					 APPLE_PIODMA_APERTURES);
	if (ret)
		return dev_err_probe(dev, ret, "missing aperture sizes\n");

	raw_spin_lock_init(&piodma->lock);
	atomic_set(&piodma->irq_epoch, 0);
	atomic_set(&piodma->irq_ack_epoch, 0);
	piodma->completed_tag = U8_MAX;
	piodma->next_tag = 1;
	ret = apple_piodma_map_buffers(piodma);
	if (ret)
		return dev_err_probe(dev, ret, "failed to allocate DMA buffers\n");
	ret = devm_add_action_or_reset(dev, apple_piodma_unmap_buffers_action,
					       piodma);
	if (ret)
		return ret;
	dev_info(dev,
		 "J700_APCIE_PIODMA_HW_INIT_DEFERRED: until=first-config-access skip=%u\n",
		 skip_hw_init);
	ret = apple_piodma_setup_unmanaged_domain(piodma);
	if (ret)
		return dev_err_probe(dev, ret,
				     "failed to create unmanaged PIODMA domain\n");
	ret = devm_add_action_or_reset(dev, apple_piodma_unmap_domain_action,
					       piodma);
	if (ret)
		return ret;
	irq = platform_get_irq(pdev, 0);
	if (irq < 0)
		return irq;
	ret = devm_request_irq(dev, irq, apple_piodma_irq, IRQF_NO_THREAD,
			       dev_name(dev), piodma);
	if (ret)
		return dev_err_probe(dev, ret, "failed to request PIODMA IRQ\n");
	platform_set_drvdata(pdev, piodma);

	dev_info(dev,
		 "J700_APCIE_PIODMA_READY: sid=17 irq=%d engine=%pa irq-aggregator=%pa fabric=%pa src=%pad dst=%pad data=%pad packet=selftest-pass completion=irq domain=unmanaged pte=cacheable topology=apple-alias iova=run275\n",
		 irq,
		 &pdev->resource[0].start, &pdev->resource[1].start,
		 &pdev->resource[2].start, &piodma->source_dma,
		 &piodma->completion_dma, &piodma->data_dma);
	return 0;
}

int apple_pcie_piodma_read(struct apple_pcie_piodma *piodma,
			   phys_addr_t address, size_t size, u32 *value,
			   bool pci_config)
{
	return apple_piodma_submit(piodma, address, size, value, false,
				   pci_config);
}
EXPORT_SYMBOL_GPL(apple_pcie_piodma_read);

int apple_pcie_piodma_write(struct apple_pcie_piodma *piodma,
			    phys_addr_t address, size_t size, u32 value,
			    bool pci_config)
{
	return apple_piodma_submit(piodma, address, size, &value, true,
				   pci_config);
}
EXPORT_SYMBOL_GPL(apple_pcie_piodma_write);

static const struct of_device_id apple_piodma_of_match[] = {
	{ .compatible = "apple,t8140-pcie-piodma" },
	{ }
};
MODULE_DEVICE_TABLE(of, apple_piodma_of_match);

static struct platform_driver apple_piodma_driver = {
	.probe = apple_piodma_probe,
	.driver = {
		.name = "pcie-apple-piodma",
		.of_match_table = apple_piodma_of_match,
		.suppress_bind_attrs = true,
	},
};
module_platform_driver(apple_piodma_driver);

MODULE_DESCRIPTION("Apple APCIe peripheral-I/O DMA transport");
MODULE_LICENSE("GPL v2");
