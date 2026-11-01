# SPDX-License-Identifier: GPL-2.0-only
# N1 transport and Wi-Fi for the APIF guest host (T6050). The module and its
# PCI driver are named apple_centauri, as before, unless the native J813/J873
# driver in ../centauri is part of the same build.
ifdef CONFIG_APPLE_CENTAURI_NATIVE
obj-$(CONFIG_APPLE_CENTAURI_APIF) += apple_centauri_apif.o
apple_centauri_apif-y := centauri_pci.o centauri_ipc.o
apple_centauri_apif-$(CONFIG_APPLE_CENTAURI_WIFI) += centauri_wifi.o
else
obj-$(CONFIG_APPLE_CENTAURI_APIF) += apple_centauri.o
apple_centauri-y := centauri_pci.o centauri_ipc.o
apple_centauri-$(CONFIG_APPLE_CENTAURI_WIFI) += centauri_wifi.o
endif
