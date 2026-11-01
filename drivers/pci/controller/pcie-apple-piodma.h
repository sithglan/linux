/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _PCIE_APPLE_PIODMA_H
#define _PCIE_APPLE_PIODMA_H

#include <linux/types.h>

struct device;
struct device_node;
struct apple_pcie_piodma;

struct apple_pcie_piodma *
apple_pcie_piodma_create(struct device *parent, struct device_node *node);

int apple_pcie_piodma_read(struct apple_pcie_piodma *piodma,
			   phys_addr_t address, size_t size, u32 *value,
			   bool pci_config);
int apple_pcie_piodma_write(struct apple_pcie_piodma *piodma,
			    phys_addr_t address, size_t size, u32 value,
			    bool pci_config);

#endif
