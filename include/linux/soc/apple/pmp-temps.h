/* SPDX-License-Identifier: GPL-2.0-only OR MIT */
/*
 * Temperature records the running PMP firmware publishes in its SRAM:
 * a NUL-terminated name, the sample (signed, 1/64 degC) at +0x34 and a
 * valid word (1) at +0x40, 0x44 bytes in all. The T8132 (J713) table is
 * Erik Bourget's and Eryk Wieliczko's (erikwb/linux j713/daily); T8142
 * (J813) has the same record format at other offsets, with the records the
 * firmware marks valid on a running J813.
 */
#ifndef _LINUX_SOC_APPLE_PMP_TEMPS_H
#define _LINUX_SOC_APPLE_PMP_TEMPS_H

#include <linux/io.h>
#include <linux/kernel.h>
#include <linux/string.h>
#include <linux/unaligned.h>

#define PMP_TEMP_RECORD_SIZE	0x44
#define PMP_TEMP_SAMPLE_OFFSET	0x34
#define PMP_TEMP_VALID_OFFSET	0x40
#define PMP_TEMP_READ_ATTEMPTS	20

/* Firmware identifiers do not establish the physical placement of sensors. */
struct apple_pmp_temp_record {
	const char *name;
	const char *label;
	u32 offset;
};

struct apple_pmp_temp_table {
	const char *compatible;
	const struct apple_pmp_temp_record *records;
	unsigned int count;
};

static const struct apple_pmp_temp_record apple_pmp_temp_records_t8132[] = {
	{ "ta000m", "Ta000m", 0x68624 },
	{ "te000m", "Te000m", 0x686c8 },
	{ "te001m", "Te001m", 0x6876c },
	{ "tp000m", "Tp000m", 0x68810 },
	{ "tp001m", "Tp001m", 0x688b4 },
	{ "tp002m", "Tp002m", 0x68958 },
};

/* 0xa4 apart, as on T8132; ta000i-ta006i and te003i read invalid. */
static const struct apple_pmp_temp_record apple_pmp_temp_records_t8142[] = {
	{ "ta007i", "Ta007i", 0x7a9a8 },
	{ "tp000i", "Tp000i", 0x7aa4c },
	{ "tp001i", "Tp001i", 0x7aaf0 },
	{ "tp003i", "Tp003i", 0x7ab94 },
	{ "tp004i", "Tp004i", 0x7ac38 },
	{ "tp00li", "Tp00li", 0x7acdc },
	{ "tp00mi", "Tp00mi", 0x7ad80 },
	{ "tp00ni", "Tp00ni", 0x7ae24 },
	{ "tp00oi", "Tp00oi", 0x7aec8 },
	{ "tp00pi", "Tp00pi", 0x7af6c },
	{ "tp00ri", "Tp00ri", 0x7b010 },
	{ "tp009i", "Tp009i", 0x7b0b4 },
	{ "tp00ai", "Tp00ai", 0x7b158 },
	{ "tp006i", "Tp006i", 0x7b1fc },
	{ "tp007i", "Tp007i", 0x7b2a0 },
	{ "te007i", "Te007i", 0x7b344 },
	{ "te001i", "Te001i", 0x7b3e8 },
	{ "te002i", "Te002i", 0x7b48c },
};

static const struct apple_pmp_temp_table apple_pmp_temp_tables[] = {
	{ "apple,t8132-pmp-v2", apple_pmp_temp_records_t8132,
	  ARRAY_SIZE(apple_pmp_temp_records_t8132) },
	{ "apple,t8142-pmp-v2", apple_pmp_temp_records_t8142,
	  ARRAY_SIZE(apple_pmp_temp_records_t8142) },
};

static inline int apple_pmp_temp_decode(const u8 *record, const char *name,
					long *value)
{
	s32 sample;

	if (memcmp(record, name, strlen(name) + 1))
		return -EIO;
	if (get_unaligned_le32(record + PMP_TEMP_VALID_OFFSET) != 1)
		return -ENODATA;

	sample = (s32)get_unaligned_le32(record + PMP_TEMP_SAMPLE_OFFSET);
	if (sample < -2560 || sample > 9600)
		return -ERANGE;

	*value = DIV_ROUND_CLOSEST((s64)sample * 1000, 64);
	return 0;
}

/* Word reads only; the record is read twice until both copies agree. */
static inline int apple_pmp_temp_sample(void __iomem *sram,
					const struct apple_pmp_temp_record *record,
					long *value)
{
	u32 first[PMP_TEMP_RECORD_SIZE / 4], second[PMP_TEMP_RECORD_SIZE / 4];
	unsigned int attempt, i;

	for (attempt = 0; attempt < PMP_TEMP_READ_ATTEMPTS; attempt++) {
		for (i = 0; i < ARRAY_SIZE(first); i++)
			first[i] = readl(sram + record->offset + 4 * i);
		for (i = 0; i < ARRAY_SIZE(second); i++)
			second[i] = readl(sram + record->offset + 4 * i);
		if (!memcmp(first, second, sizeof(first)))
			return apple_pmp_temp_decode((const u8 *)first,
						     record->name, value);
	}

	return -EAGAIN;
}

#endif
