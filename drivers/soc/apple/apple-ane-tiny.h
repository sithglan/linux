/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef _APPLE_ANE_TINY_H
#define _APPLE_ANE_TINY_H

/* Host-side embedding lookup and greedy token selection for the fixed
 * 512-vocabulary, 64-channel, 32-position experiment. No kernel FP use.
 * The transformer layers and output projection execute on the ANE.
 */
static inline int apple_ane_tiny_input(const u8 *embeddings, u32 embedding_size,
				      const u32 *tokens, u32 count, u8 *input, u32 size)
{
	u32 channel, position, token;

	if (!embeddings || !tokens || !input || embedding_size != 512 * 64 * 2 ||
	    size != 64 * 32 * 2 || !count || count > 32)
		return -EINVAL;
	/* Validate all tokens before changing the DMA buffer. */
	for (position = 0; position < count; position++)
		if (tokens[position] >= 512)
			return -ERANGE;
	for (channel = 0; channel < 64; channel++)
		for (position = 0; position < 32; position++) {
			token = position < count ? tokens[position] : 0;
			memcpy(input + (channel * 32 + position) * 2,
			       embeddings + (token * 64 + channel) * 2, 2);
		}
	return 0;
}

static inline int apple_ane_tiny_argmax(const u8 *output, u32 size,
				       u32 position, u32 *token)
{
	u32 i, bits, key, best = 0, selected = 0;

	if (!output || !token || size != 512 * 32 * 2 || position >= 32)
		return -EINVAL;
	for (i = 0; i < 512; i++) {
		bits = get_unaligned_le16(output + (i * 32 + position) * 2);
		if ((bits & 0x7c00) == 0x7c00)
			return -EILSEQ;
		/* Equal signed zeros choose the first token, as the CPU reference. */
		if (!(bits & 0x7fff))
			bits = 0;
		key = bits & 0x8000 ? (~bits & 0xffff) : (bits | 0x8000);
		if (!i || key > best) {
			best = key;
			selected = i;
		}
	}
	*token = selected;
	return 0;
}

#endif
