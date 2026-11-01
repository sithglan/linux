# SPDX-License-Identifier: GPL-2.0
# N1 Beta HCI transport for the APIF guest host (T6050). The module and its
# PCI driver are named btcentauri, as before, unless the native J813/J873
# btcentauri (CONFIG_BT_CENTAURI) is part of the same build.
ifdef CONFIG_BT_CENTAURI
obj-$(CONFIG_BT_HCICENTAURI) += btcentauri_apif.o
btcentauri_apif-y := btcentauri.o
else
obj-$(CONFIG_BT_HCICENTAURI) += btcentauri.o
endif
