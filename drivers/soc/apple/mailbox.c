// SPDX-License-Identifier: GPL-2.0-only OR MIT
/*
 * Apple mailbox driver
 *
 * Copyright The Asahi Linux Contributors
 *
 * This driver adds support for two mailbox variants (called ASC and M3 by
 * Apple) found in Apple SoCs such as the M1. It consists of two FIFOs used to
 * exchange 64+32 bit messages between the main CPU and a co-processor.
 * Various coprocessors implement different IPC protocols based on these simple
 * messages and shared memory buffers.
 *
 * Both the main CPU and the co-processor see the same set of registers but
 * the first FIFO (A2I) is always used to transfer messages from the application
 * processor (us) to the I/O processor and the second one (I2A) for the
 * other direction.
 */

#include <linux/bitfield.h>
#include <linux/bits.h>
#include <linux/delay.h>
#include <linux/device.h>
#include <linux/interrupt.h>
#include <linux/io.h>
#include <linux/iopoll.h>
#include <linux/module.h>
#include <linux/of.h>
#include <linux/of_platform.h>
#include <linux/platform_device.h>
#include <linux/pm_runtime.h>
#include <linux/soc/apple/mailbox.h>
#include <linux/spinlock.h>
#include <linux/types.h>
#include <linux/workqueue.h>

#define APPLE_ASC_MBOX_CONTROL_FULL BIT(16)
#define APPLE_ASC_MBOX_CONTROL_EMPTY BIT(17)

#define APPLE_ASC_MBOX_A2I_CONTROL 0x110
#define APPLE_ASC_MBOX_A2I_SEND0 0x800
#define APPLE_ASC_MBOX_A2I_SEND1 0x808
#define APPLE_ASC_MBOX_A2I_RECV0 0x810
#define APPLE_ASC_MBOX_A2I_RECV1 0x818

#define APPLE_ASC_MBOX_I2A_CONTROL 0x114
#define APPLE_ASC_MBOX_I2A_SEND0 0x820
#define APPLE_ASC_MBOX_I2A_SEND1 0x828
#define APPLE_ASC_MBOX_I2A_RECV0 0x830
#define APPLE_ASC_MBOX_I2A_RECV1 0x838

/* ASCWrap v6 embeds the unchanged ASC mailbox block at this offset. */
#define APPLE_ASCWRAP_V6_MBOX_OFFSET 0x8000
#define APPLE_ASCWRAP_V6_WRAPPER_SIZE 0x88000
#define APPLE_ASCWRAP_V6_IOP_VBAR_SIZE 0x8
#define APPLE_ASCWRAP_V6_CPU_CONTROL 0x44
#define APPLE_ASCWRAP_V6_CPU_STATUS 0x48
#define APPLE_ASCWRAP_V6_CPU_RUN BIT(4)
#define APPLE_ASCWRAP_V6_CPU_STOP_SECOND_CLEAR BIT(5)

#define APPLE_T8140_GFX_WRAPPER_BASE 0x482600000ULL
#define APPLE_T8140_GFX_IOP_VBAR_BASE 0x482050000ULL
#define APPLE_T8140_GFX1_WRAPPER_BASE 0x482e00000ULL
#define APPLE_T8140_GFX1_IOP_VBAR_BASE 0x482850000ULL
#define APPLE_T8142_ASCWRAP_BASE_DELTA 0x100000000ULL
#define APPLE_T6050_ASCWRAP_BASE_DELTA 0x380000000ULL

/*
 * T8152 (J873) GPU ASCWrap v8 providers. The ADT gfx-asc/gfx1-asc nodes are
 * iop,ascwrap-v8 with reg[0] = wrapper (0x74000), reg[1] = iop-vbar (8) and
 * reg[2] = the post-office bank 0x316020000/0x10000, which is shared with
 * PMP, DCP, DCPEXT0/1 and SCALER0. Only the GFX/GFX1 0x80-byte windows of
 * that bank are claimed: offset (2 * msgbox-mailbox-num + !postoffice) *
 * 0x80 with msgbox-using-alias=1 (m1n1 asc.c) gives 0x316020100 (mailbox 1)
 * and 0x316020200 (mailbox 2). Each window uses the counted-FIFO layout of
 * apple_mbox_t8152_v8_hw (RX +0, TX +0x10, status +0x20).
 *
 * The wrapper is only touched through the lifecycle helpers, and every one
 * of those accesses is preceded by a read of the block's PMGR2 PSD service
 * (implicit services 81/82 at 0x303a15100/0x303a15200: state +0x80, ack
 * +0xbc). The provider never writes PSD registers.
 *
 * Receive side. AppleASCWrapV8 has no mailbox-level interrupt control
 * (_enableInboxInterrupt/_enableOutboxInterrupt/_disableAllInterrupts are
 * empty), so has_irq_controls is false and a level interrupt can only be
 * masked at the AIC. The ADT interrupt order is proven from the kext
 * (_inboxEmptyInterruptIndex() = 0, _outboxNotEmptyInterruptIndex() = 1)
 * but not yet observed for GFX. By default the provider therefore never
 * enables either GPU mailbox interrupt: apple_mbox_start() starts a
 * one-jiffy receive poller, and a full TX FIFO is waited out by sleeping
 * polls. Only ascwrap_v8_rx_irq=1 (requires user approval) uses the
 * recv-not-empty line; even then a line that keeps firing with nothing to
 * read is disabled and receive falls back to the poller. Every post-office
 * receive poll is preceded by the same PSD proof as the wrapper accesses.
 */
#define APPLE_ASCWRAP_V8_WRAPPER_SIZE 0x74000
#define APPLE_ASCWRAP_V8_IOP_VBAR_SIZE 0x8
#define APPLE_ASCWRAP_V8_MBOX_SIZE 0x80
#define APPLE_ASCWRAP_V8_PSD_SIZE 0x100
/* m1n1 asc.c drives v8 CPU start at reg[0] + 0x44 bit 4 (INFERRED for GFX). */
#define APPLE_ASCWRAP_V8_CPU_CONTROL 0x44
#define APPLE_ASCWRAP_V8_CPU_STATUS 0x48
#define APPLE_ASCWRAP_V8_CPU_RUN BIT(4)
#define APPLE_PSD_STATE 0x80
#define APPLE_PSD_ACK 0xbc
#define APPLE_PSD_STATE_MASK GENMASK(2, 0)
#define APPLE_T8152_GPU_ASC_PSD_ACTIVE 2
/* apple_mbox_ascwrap_v6_lifecycle.missing bit: CPU control not approved. */
#define APPLE_ASCWRAP_V8_MISSING_CPU_CONTROL_APPROVAL BIT(0)
/* Receive poll period (poll-only mode, or after the storm guard tripped). */
#define APPLE_ASCWRAP_V8_RX_POLL_JIFFIES 1
/* Consecutive recv interrupts that drained nothing before the line is cut. */
#define APPLE_ASCWRAP_V8_RX_STORM_LIMIT 1000

#define APPLE_T8152_GFX_WRAPPER_BASE 0x582600000ULL
#define APPLE_T8152_GFX_IOP_VBAR_BASE 0x582050000ULL
#define APPLE_T8152_GFX_MBOX_BASE 0x316020100ULL
#define APPLE_T8152_GFX_PSD_BASE 0x303a15100ULL
#define APPLE_T8152_GFX1_WRAPPER_BASE 0x582e00000ULL
#define APPLE_T8152_GFX1_IOP_VBAR_BASE 0x582850000ULL
#define APPLE_T8152_GFX1_MBOX_BASE 0x316020200ULL
#define APPLE_T8152_GFX1_PSD_BASE 0x303a15200ULL

/*
 * REQUIRES USER APPROVAL. Writing ASC CPU control on a J873 GPU wrapper is
 * an unqualified power-affecting operation. The default keeps every v8 CPU
 * start/stop request refused (-EPERM); only lifecycle reads are allowed.
 */
static bool ascwrap_v8_cpu_control;
module_param(ascwrap_v8_cpu_control, bool, 0444);
MODULE_PARM_DESC(ascwrap_v8_cpu_control,
		 "REQUIRES USER APPROVAL: allow T8152 GPU ASCWrap v8 CPU start/stop writes (default: refuse)");

/*
 * REQUIRES USER APPROVAL. The GFX/GFX1 interrupt assignment is not observed
 * on hardware, and a wrongly wired level line would storm with interrupts
 * that the handler cannot mask. The default services receive by polling and
 * never enables a GPU mailbox interrupt.
 */
static bool ascwrap_v8_rx_irq;
module_param(ascwrap_v8_rx_irq, bool, 0444);
MODULE_PARM_DESC(ascwrap_v8_rx_irq,
		 "REQUIRES USER APPROVAL: service T8152 GPU ASCWrap v8 receive with the recv-not-empty interrupt (default: poll, no GPU mailbox interrupt is enabled)");

static_assert(APPLE_ASCWRAP_V6_MBOX_OFFSET + APPLE_ASC_MBOX_A2I_CONTROL ==
	      0x8110);
static_assert(APPLE_ASCWRAP_V6_MBOX_OFFSET + APPLE_ASC_MBOX_I2A_CONTROL ==
	      0x8114);
static_assert(APPLE_ASCWRAP_V6_MBOX_OFFSET + APPLE_ASC_MBOX_A2I_SEND0 ==
	      0x8800);
static_assert(APPLE_ASCWRAP_V6_MBOX_OFFSET + APPLE_ASC_MBOX_I2A_RECV0 ==
	      0x8830);

/*
 * The T8140 AOP "setup port" (ADT aop-exclave-mailbox): the second mailbox
 * the J700 AOP firmware boots through.  Layout measured on hardware by the
 * native m1n1 microphone host (artifacts/j700-native-lpmic-20260906).
 */
#define APPLE_T8140_AOP_SETUP_A2I_CONTROL 0x000
#define APPLE_T8140_AOP_SETUP_I2A_CONTROL 0x004
#define APPLE_T8140_AOP_SETUP_A2I_SEND0   0x180
#define APPLE_T8140_AOP_SETUP_A2I_SEND1   0x188
#define APPLE_T8140_AOP_SETUP_I2A_RECV0   0x1b0
#define APPLE_T8140_AOP_SETUP_I2A_RECV1   0x1b8

#define APPLE_T8015_MBOX_A2I_CONTROL	0x108
#define APPLE_T8015_MBOX_I2A_CONTROL	0x10c

#define APPLE_M3_MBOX_CONTROL_FULL BIT(16)
#define APPLE_M3_MBOX_CONTROL_EMPTY BIT(17)

#define APPLE_M3_MBOX_A2I_CONTROL 0x50
#define APPLE_M3_MBOX_A2I_SEND0 0x60
#define APPLE_M3_MBOX_A2I_SEND1 0x68
#define APPLE_M3_MBOX_A2I_RECV0 0x70
#define APPLE_M3_MBOX_A2I_RECV1 0x78

#define APPLE_M3_MBOX_I2A_CONTROL 0x80
#define APPLE_M3_MBOX_I2A_SEND0 0x90
#define APPLE_M3_MBOX_I2A_SEND1 0x98
#define APPLE_M3_MBOX_I2A_RECV0 0xa0
#define APPLE_M3_MBOX_I2A_RECV1 0xa8

#define APPLE_M3_MBOX_IRQ_ENABLE 0x48
#define APPLE_M3_MBOX_IRQ_ACK 0x4c
#define APPLE_M3_MBOX_IRQ_A2I_EMPTY BIT(0)
#define APPLE_M3_MBOX_IRQ_A2I_NOT_EMPTY BIT(1)
#define APPLE_M3_MBOX_IRQ_I2A_EMPTY BIT(2)
#define APPLE_M3_MBOX_IRQ_I2A_NOT_EMPTY BIT(3)

#define APPLE_MBOX_MSG1_OUTCNT GENMASK(56, 52)
#define APPLE_MBOX_MSG1_INCNT GENMASK(51, 48)
#define APPLE_MBOX_MSG1_OUTPTR GENMASK(47, 44)
#define APPLE_MBOX_MSG1_INPTR GENMASK(43, 40)
#define APPLE_MBOX_MSG1_MSG GENMASK(31, 0)

#define APPLE_MBOX_TX_TIMEOUT 500

struct apple_mbox_hw {
	unsigned int reg_offset;
	bool is_ascwrap_v6;
	/* T8152 GPU ASCWrap v8: wrapper + iop-vbar + post-office window + PSD. */
	bool is_ascwrap_v8;
	/* PMP's postoffice is qualified; its IRQ ordering is not yet known. */
	bool rx_poll_required;
	/* J873 post-office window a polled provider must sit at. */
	u64 j873_window;
	bool counted_fifo;
	/* ASCWrap v6 offsets from the T8140 IOP VBAR and wrapper bases. */
	u64 ascwrap_v6_base_delta;
	u64 ascwrap_v6_wrapper_delta;
	bool ap_initializes_mailboxes;

	unsigned int control_full;
	unsigned int control_empty;

	unsigned int a2i_control;
	unsigned int a2i_send0;
	unsigned int a2i_send1;

	unsigned int i2a_control;
	unsigned int i2a_recv0;
	unsigned int i2a_recv1;

	bool has_irq_controls;
	unsigned int irq_enable;
	unsigned int irq_ack;
	unsigned int irq_bit_recv_not_empty;
	unsigned int irq_bit_send_empty;
};

/*
 * Provider-private state kept outside the consumer-visible struct apple_mbox.
 * Every provider is allocated as one of these; apple_mbox_get() hands out a
 * pointer to the embedded mailbox.
 */
struct apple_mbox_priv {
	struct apple_mbox mbox;
	/* ASCWrap v8 only: the provider's PMGR2 PSD service block (read-only). */
	void __iomem *psd_regs;
	/* ASCWrap v8 only: receive by polling; fixed at probe. */
	bool rx_poll_only;
	/* rx_lock: recv interrupt enabled and not yet disabled again. */
	bool rx_irq_live;
	/* rx_lock: consecutive recv interrupts that drained nothing. */
	unsigned int rx_irq_empty_run;
	/* rx_lock: an unpowered-PSD receive skip was logged this activation. */
	bool rx_psd_skip_logged;
	struct delayed_work rx_poll_work;
};

static struct apple_mbox_priv *apple_mbox_to_priv(struct apple_mbox *mbox)
{
	return container_of(mbox, struct apple_mbox_priv, mbox);
}

static bool apple_mbox_has_rx_poller(const struct apple_mbox *mbox)
{
	return mbox->hw->is_ascwrap_v8 || mbox->hw->rx_poll_required;
}

/* Both ASCWrap generations share the consumer lifecycle API below. */
static bool apple_mbox_has_cpu_lifecycle(const struct apple_mbox *mbox)
{
	return mbox->hw->is_ascwrap_v6 || mbox->hw->is_ascwrap_v8;
}

/*
 * Read-only proof that a v8 ASC block is powered before any wrapper MMIO.
 * The exact T8152 GFX/GFX1 providers use state 2, matching native 26A428.
 * J873 hardware confirms reading STATE clears the transition ACK: snapshot
 * ACK first, but do not require it to remain set for steady-state access.
 * Cold state 6 is rejected. Never writes the PSD request register.
 */
static int apple_mbox_ascwrap_v8_psd_powered(struct apple_mbox *mbox,
					     u32 *state, u32 *ack)
{
	void __iomem *psd = apple_mbox_to_priv(mbox)->psd_regs;

	*state = 0;
	*ack = 0;
	if (!mbox->hw->is_ascwrap_v8 || !psd)
		return -ENODEV;

	*ack = readl(psd + APPLE_PSD_ACK);
	*state = readl(psd + APPLE_PSD_STATE);
	if (FIELD_GET(APPLE_PSD_STATE_MASK, *state) != APPLE_T8152_GPU_ASC_PSD_ACTIVE)
		return -EHOSTDOWN;

	return 0;
}

int apple_mbox_ascwrap_v6_get_lifecycle(
	struct apple_mbox *mbox,
	struct apple_mbox_ascwrap_v6_lifecycle *lifecycle)
{
	if (!mbox || !lifecycle)
		return -EINVAL;
	if (READ_ONCE(mbox->removing))
		return -ENODEV;
	if (!apple_mbox_has_cpu_lifecycle(mbox))
		return -EOPNOTSUPP;

	*lifecycle = mbox->ascwrap_v6;
	return 0;
}
EXPORT_SYMBOL_GPL(apple_mbox_ascwrap_v6_get_lifecycle);

int apple_mbox_ascwrap_v6_require_safe_cpu_lifecycle(struct apple_mbox *mbox)
{
	if (!mbox || !apple_mbox_has_cpu_lifecycle(mbox))
		return -EOPNOTSUPP;
	if (READ_ONCE(mbox->removing))
		return -ENODEV;

	/* Future provider revisions may report additional lifecycle work. */
	if (mbox->ascwrap_v6.missing)
		return -EOPNOTSUPP;

	return 0;
}
EXPORT_SYMBOL_GPL(apple_mbox_ascwrap_v6_require_safe_cpu_lifecycle);

static void apple_mbox_ascwrap_v6_stop_cpu_locked(struct apple_mbox *mbox)
{
	void __iomem *cpu_control;
	u32 val;

	if (!mbox->ascwrap_v6_cpu_running)
		return;

	if (mbox->hw->is_ascwrap_v8) {
		u32 state, ack;

		/*
		 * Reached only after an approved v8 start. m1n1 asc_cpu_stop():
		 * clear RUN at +0x44. An unpowered block cannot be running, and
		 * touching its wrapper would fault, so skip the write then.
		 */
		cpu_control = (u8 __iomem *)mbox->wrapper_regs +
			      APPLE_ASCWRAP_V8_CPU_CONTROL;
		if (apple_mbox_ascwrap_v8_psd_powered(mbox, &state, &ack)) {
			dev_warn(mbox->dev,
				 "ASCWrap v8 PSD state=%#x ack=%#x is not powered; CPU stop write skipped\n",
				 state, ack);
		} else {
			val = readl(cpu_control);
			writel(val & ~APPLE_ASCWRAP_V8_CPU_RUN, cpu_control);
		}
	} else {
		cpu_control = (u8 __iomem *)mbox->wrapper_regs +
			      APPLE_ASCWRAP_V6_CPU_CONTROL;

		/* Stop: clear RUN, reread, clear bit 5. */
		val = readl(cpu_control);
		writel(val & ~APPLE_ASCWRAP_V6_CPU_RUN, cpu_control);
		val = readl(cpu_control);
		writel(val & ~APPLE_ASCWRAP_V6_CPU_STOP_SECOND_CLEAR, cpu_control);
	}

	mbox->ascwrap_v6_cpu_running = false;
	pm_runtime_mark_last_busy(mbox->dev);
	pm_runtime_put_autosuspend(mbox->dev);
}

/*
 * A failed resume leaves the runtime PM error set, and runtime PM then fails
 * every later resume with -EINVAL without retrying the power domain.  The
 * provider keeps no state across a failed resume, so mark it suspended again
 * and let the next start retry the domain.
 */
static void apple_mbox_clear_resume_error(struct apple_mbox *mbox)
{
	if (mbox->dev->power.runtime_error)
		pm_runtime_set_suspended(mbox->dev);
}

int apple_mbox_ascwrap_v6_start_cpu(struct apple_mbox *mbox)
{
	void __iomem *cpu_control;
	u32 val;
	int ret = 0;

	if (!mbox || !apple_mbox_has_cpu_lifecycle(mbox) || !mbox->wrapper_regs)
		return -EOPNOTSUPP;
	/* REQUIRES USER APPROVAL: see the ascwrap_v8_cpu_control parameter. */
	if (mbox->hw->is_ascwrap_v8 && !ascwrap_v8_cpu_control)
		return -EPERM;

	mutex_lock(&mbox->lifecycle_lock);
	if (mbox->removing) {
		ret = -ENODEV;
		goto out_unlock;
	}
	if (mbox->ascwrap_v6.missing) {
		ret = -EOPNOTSUPP;
		goto out_unlock;
	}
	if (mbox->ascwrap_v6_cpu_running) {
		ret = -EBUSY;
		goto out_unlock;
	}
	if (mbox->hw->is_ascwrap_v8) {
		u32 state, ack;

		ret = apple_mbox_ascwrap_v8_psd_powered(mbox, &state, &ack);
		if (ret) {
			dev_err(mbox->dev,
				"ASCWrap v8 PSD state=%#x ack=%#x is not powered; CPU start refused\n",
				state, ack);
			goto out_unlock;
		}
	}

	ret = pm_runtime_resume_and_get(mbox->dev);
	if (ret) {
		dev_err(mbox->dev,
			"ASC CPU start: runtime resume failed: %d (active=%d status=%d disable_depth=%d runtime_error=%d)\n",
			ret, mbox->active, mbox->dev->power.runtime_status,
			mbox->dev->power.disable_depth,
			mbox->dev->power.runtime_error);
		apple_mbox_clear_resume_error(mbox);
		goto out_unlock;
	}

	if (mbox->hw->is_ascwrap_v8) {
		/* m1n1 asc_cpu_start(): set RUN at reg[0] + 0x44. */
		cpu_control = (u8 __iomem *)mbox->wrapper_regs +
			      APPLE_ASCWRAP_V8_CPU_CONTROL;
		val = readl(cpu_control);
		writel(val | APPLE_ASCWRAP_V8_CPU_RUN, cpu_control);
	} else {
		cpu_control = (u8 __iomem *)mbox->wrapper_regs +
			      APPLE_ASCWRAP_V6_CPU_CONTROL;

		/* Start: read, set RUN, write. */
		val = readl(cpu_control);
		writel(val | APPLE_ASCWRAP_V6_CPU_RUN, cpu_control);
	}
	mbox->ascwrap_v6_cpu_running = true;

out_unlock:
	mutex_unlock(&mbox->lifecycle_lock);
	return ret;
}
EXPORT_SYMBOL_GPL(apple_mbox_ascwrap_v6_start_cpu);

int apple_mbox_ascwrap_v6_stop_cpu(struct apple_mbox *mbox)
{
	int ret = 0;

	if (!mbox || !apple_mbox_has_cpu_lifecycle(mbox) || !mbox->wrapper_regs)
		return -EOPNOTSUPP;
	/* REQUIRES USER APPROVAL: see the ascwrap_v8_cpu_control parameter. */
	if (mbox->hw->is_ascwrap_v8 && !ascwrap_v8_cpu_control)
		return -EPERM;

	mutex_lock(&mbox->lifecycle_lock);
	if (mbox->removing) {
		ret = -ENODEV;
		goto out_unlock;
	}

	/* A no-op unless this provider started the CPU itself. */
	apple_mbox_ascwrap_v6_stop_cpu_locked(mbox);

out_unlock:
	mutex_unlock(&mbox->lifecycle_lock);
	return ret;
}
EXPORT_SYMBOL_GPL(apple_mbox_ascwrap_v6_stop_cpu);

int apple_mbox_ascwrap_v6_get_cpu_state(struct apple_mbox *mbox,
				     u32 *control, u32 *status)
{
	int ret = 0;

	if (!mbox || !control || !status || !apple_mbox_has_cpu_lifecycle(mbox) ||
	    !mbox->wrapper_regs)
		return -EOPNOTSUPP;

	mutex_lock(&mbox->lifecycle_lock);
	if (mbox->removing) {
		ret = -ENODEV;
	} else if (mbox->hw->is_ascwrap_v8) {
		u32 state, ack;

		/* PSD proof first; an unpowered wrapper read would SError. */
		*control = 0;
		*status = 0;
		ret = apple_mbox_ascwrap_v8_psd_powered(mbox, &state, &ack);
		if (ret) {
			dev_info_ratelimited(mbox->dev,
					     "ASCWrap v8 PSD state=%#x ack=%#x is not powered; CPU_CONTROL/STATUS reads skipped\n",
					     state, ack);
		} else {
			*control = readl((u8 __iomem *)mbox->wrapper_regs +
					 APPLE_ASCWRAP_V8_CPU_CONTROL);
			*status = readl((u8 __iomem *)mbox->wrapper_regs +
					APPLE_ASCWRAP_V8_CPU_STATUS);
			dev_dbg(mbox->dev,
				 "ASCWrap v8 PSD state=%#x ack=%#x CPU_CONTROL=%#x CPU_STATUS=%#x\n",
				 state, ack, *control, *status);
		}
	} else {
		*control = readl((u8 __iomem *)mbox->wrapper_regs +
				APPLE_ASCWRAP_V6_CPU_CONTROL);
		*status = readl((u8 __iomem *)mbox->wrapper_regs +
			       APPLE_ASCWRAP_V6_CPU_STATUS);
	}
	mutex_unlock(&mbox->lifecycle_lock);
	return ret;
}
EXPORT_SYMBOL_GPL(apple_mbox_ascwrap_v6_get_cpu_state);

static bool apple_mbox_tx_full(struct apple_mbox *mbox, u32 status)
{
	if (mbox->hw->counted_fifo)
		return !(status & mbox->hw->control_full);
	return status & mbox->hw->control_full;
}

static bool apple_mbox_rx_empty(struct apple_mbox *mbox, u32 status)
{
	if (mbox->hw->counted_fifo)
		return !(status & mbox->hw->control_empty);
	return status & mbox->hw->control_empty;
}

/* ASCWrap v8 transfers each message with one paired access. Its status word
 * contains RX occupancy and free TX slots, rather than full/empty flags.
 * Keep the two words together, as on the qualified native T8152 path.
 */
static void apple_mbox_write_msg(struct apple_mbox *mbox,
			       struct apple_mbox_msg msg)
{
	u64 high = FIELD_PREP(APPLE_MBOX_MSG1_MSG, msg.msg1);

#ifdef CONFIG_ARM64
	if (mbox->hw->counted_fifo) {
		u64 low = (__force u64)cpu_to_le64(msg.msg0);

		high = (__force u64)cpu_to_le64(high);
		wmb();
		asm volatile("stp %0, %1, [%2]"
			     : : "r" (low), "r" (high),
				 "r" (mbox->regs + mbox->hw->a2i_send0)
			     : "memory");
	} else
#endif
	{
		writeq_relaxed(msg.msg0, mbox->regs + mbox->hw->a2i_send0);
		writeq_relaxed(high, mbox->regs + mbox->hw->a2i_send1);
	}
}

static struct apple_mbox_msg apple_mbox_read_msg(struct apple_mbox *mbox)
{
	struct apple_mbox_msg msg;
	u64 low, high;

#ifdef CONFIG_ARM64
	if (mbox->hw->counted_fifo) {
		asm volatile("ldp %0, %1, [%2]"
			     : "=&r" (low), "=&r" (high)
			     : "r" (mbox->regs + mbox->hw->i2a_recv0)
			     : "memory");
		rmb();
		low = le64_to_cpu((__force __le64)low);
		high = le64_to_cpu((__force __le64)high);
	} else
#endif
	{
		low = readq_relaxed(mbox->regs + mbox->hw->i2a_recv0);
		high = readq_relaxed(mbox->regs + mbox->hw->i2a_recv1);
	}
	msg.msg0 = low;
	msg.msg1 = FIELD_GET(APPLE_MBOX_MSG1_MSG, high);
	return msg;
}

int apple_mbox_send(struct apple_mbox *mbox, const struct apple_mbox_msg msg,
		    bool atomic)
{
	unsigned long flags;
	unsigned long deadline = 0;
	int ret;
	u32 mbox_ctrl;
	long t;

	spin_lock_irqsave(&mbox->tx_lock, flags);
	if (unlikely(READ_ONCE(mbox->removing))) {
		spin_unlock_irqrestore(&mbox->tx_lock, flags);
		return -ENODEV;
	}
	mbox_ctrl = readl_relaxed(mbox->regs + mbox->hw->a2i_control);

	while (apple_mbox_tx_full(mbox, mbox_ctrl)) {
		/*
		 * Poll-only providers (v8 by default, and the J873 post-office
		 * mailboxes, which set irq_send_empty < 0) sleep-poll here, so
		 * this check must come before the busy-poll below.
		 */
		if (!atomic && apple_mbox_has_rx_poller(mbox) &&
		    apple_mbox_to_priv(mbox)->rx_poll_only) {
			/*
			 * Poll-only v8: never enable the send-empty line.
			 * Sleep-poll the TX status, touching MMIO only under
			 * tx_lock so that removal cannot unmap it underneath.
			 */
			if (!deadline)
				deadline = jiffies +
					   msecs_to_jiffies(APPLE_MBOX_TX_TIMEOUT);
			spin_unlock_irqrestore(&mbox->tx_lock, flags);
			if (time_after(jiffies, deadline))
				return -ETIMEDOUT;
			usleep_range(100, 200);
			spin_lock_irqsave(&mbox->tx_lock, flags);
			if (unlikely(READ_ONCE(mbox->removing))) {
				spin_unlock_irqrestore(&mbox->tx_lock, flags);
				return -ENODEV;
			}
			mbox_ctrl = readl_relaxed(mbox->regs +
						  mbox->hw->a2i_control);
			continue;
		}
		/* No send-empty IRQ (optional, e.g. the T6021 ANE ASC): poll. */
		if (atomic || mbox->irq_send_empty < 0) {
			ret = readl_poll_timeout_atomic(
				mbox->regs + mbox->hw->a2i_control, mbox_ctrl,
				!apple_mbox_tx_full(mbox, mbox_ctrl), 100,
				APPLE_MBOX_TX_TIMEOUT * 1000);

			if (ret) {
				spin_unlock_irqrestore(&mbox->tx_lock, flags);
				return ret;
			}

			break;
		}
		/*
		 * The interrupt is level triggered and will keep firing as long as the
		 * FIFO is empty. It will also keep firing if the FIFO was empty
		 * at any point in the past until it has been acknowledged at the
		 * mailbox level. By acknowledging it here we can ensure that we will
		 * only get the interrupt once the FIFO has been cleared again.
		 * If the FIFO is already empty before the ack it will fire again
		 * immediately after the ack.
		 */
		if (mbox->hw->has_irq_controls) {
			writel_relaxed(mbox->hw->irq_bit_send_empty,
				       mbox->regs + mbox->hw->irq_ack);
		}
		reinit_completion(&mbox->tx_empty);
		if (!mbox->tx_irq_unmasked) {
			mbox->tx_irq_unmasked = true;
			enable_irq(mbox->irq_send_empty);
		}
		spin_unlock_irqrestore(&mbox->tx_lock, flags);

		t = wait_for_completion_interruptible_timeout(
			&mbox->tx_empty,
			msecs_to_jiffies(APPLE_MBOX_TX_TIMEOUT));
		if (t <= 0) {
			u32 a2i = 0, i2a = 0;
			bool removing;

			/* A late IRQ may already have masked this under tx_lock. */
			spin_lock_irqsave(&mbox->tx_lock, flags);
			removing = READ_ONCE(mbox->removing);
			if (!removing) {
				a2i = readl_relaxed(mbox->regs + mbox->hw->a2i_control);
				i2a = readl_relaxed(mbox->regs + mbox->hw->i2a_control);
			}
			if (mbox->tx_irq_unmasked) {
				disable_irq_nosync(mbox->irq_send_empty);
				mbox->tx_irq_unmasked = false;
			}
			spin_unlock_irqrestore(&mbox->tx_lock, flags);
			dev_err(mbox->dev,
				"TX wait failed ret=%ld a2i=%#x i2a=%#x full=%#x send_irq=%d recv_irq=%d removing=%d msg=%#llx/%#x\n",
				t, a2i, i2a, mbox->hw->control_full,
				mbox->irq_send_empty, mbox->irq_recv_not_empty,
				removing, msg.msg0, msg.msg1);
			return t < 0 ? t : -ETIMEDOUT;
		}

		spin_lock_irqsave(&mbox->tx_lock, flags);
		if (unlikely(READ_ONCE(mbox->removing))) {
			spin_unlock_irqrestore(&mbox->tx_lock, flags);
			return -ENODEV;
		}
		mbox_ctrl = readl_relaxed(mbox->regs + mbox->hw->a2i_control);
	}

	apple_mbox_write_msg(mbox, msg);

	spin_unlock_irqrestore(&mbox->tx_lock, flags);

	return 0;
}
EXPORT_SYMBOL(apple_mbox_send);

static irqreturn_t apple_mbox_send_empty_irq(int irq, void *data)
{
	struct apple_mbox *mbox = data;

	/*
	 * We don't need to acknowledge the interrupt at the mailbox level
	 * here even if supported by the hardware. It will keep firing but that
	 * doesn't matter since it's disabled at the main interrupt controller.
	 * apple_mbox_send will acknowledge it before enabling
	 * it at the main controller again.
	 */
	spin_lock(&mbox->tx_lock);
	if (mbox->tx_irq_unmasked) {
		disable_irq_nosync(mbox->irq_send_empty);
		mbox->tx_irq_unmasked = false;
	}
	complete(&mbox->tx_empty);
	spin_unlock(&mbox->tx_lock);

	return IRQ_HANDLED;
}

static int apple_mbox_poll_locked(struct apple_mbox *mbox)
{
	struct apple_mbox_msg msg;
	int ret = 0;
	u32 mbox_ctrl;

	/* A stopped receiver may still have firmware traffic in its FIFO. */
	if (!mbox->rx)
		return 0;
	if (mbox->hw->is_ascwrap_v8) {
		struct apple_mbox_priv *priv = apple_mbox_to_priv(mbox);
		u32 state, ack;

		/* PSD proof before the post-office status read. */
		ret = apple_mbox_ascwrap_v8_psd_powered(mbox, &state, &ack);
		if (ret) {
			if (!priv->rx_psd_skip_logged)
				dev_info(mbox->dev,
					 "ASCWrap v8 PSD state=%#x ack=%#x is not powered; receive poll skipped\n",
					 state, ack);
			priv->rx_psd_skip_logged = true;
			return ret;
		}
	}

	mbox_ctrl = readl_relaxed(mbox->regs + mbox->hw->i2a_control);

	while (!apple_mbox_rx_empty(mbox, mbox_ctrl)) {
		msg = apple_mbox_read_msg(mbox);

		mbox->rx(mbox, msg, mbox->cookie);
		ret++;
		mbox_ctrl = readl_relaxed(mbox->regs + mbox->hw->i2a_control);
	}

	/*
	 * The interrupt will keep firing even if there are no more messages
	 * unless we also acknowledge it at the mailbox level here.
	 * There's no race if a message comes in between the check in the while
	 * loop above and the ack below: If a new messages arrives inbetween
	 * those two the interrupt will just fire again immediately after the
	 * ack since it's level triggered.
	 */
	if (mbox->hw->has_irq_controls) {
		writel_relaxed(mbox->hw->irq_bit_recv_not_empty,
			       mbox->regs + mbox->hw->irq_ack);
	}

	return ret;
}

/*
 * ASCWrap v8 storm guard, rx_lock held. The handler cannot mask the line at
 * the mailbox (no irq controls), so a line that keeps firing with nothing to
 * read (for example a send-empty line wired as recv-not-empty) would lock up
 * the CPU while always returning IRQ_HANDLED. Cut it at the AIC and poll.
 */
static void apple_mbox_v8_rx_irq_guard(struct apple_mbox *mbox, int drained)
{
	struct apple_mbox_priv *priv = apple_mbox_to_priv(mbox);

	if (drained > 0) {
		priv->rx_irq_empty_run = 0;
		return;
	}
	if (!priv->rx_irq_live ||
	    ++priv->rx_irq_empty_run < APPLE_ASCWRAP_V8_RX_STORM_LIMIT)
		return;

	priv->rx_irq_live = false;
	disable_irq_nosync(mbox->irq_recv_not_empty);
	dev_err(mbox->dev,
		"ASCWrap v8 recv-not-empty IRQ %d fired %u times without a message; disabled, receive falls back to polling\n",
		mbox->irq_recv_not_empty, priv->rx_irq_empty_run);
	queue_delayed_work(system_highpri_wq, &priv->rx_poll_work, 0);
}

static irqreturn_t apple_mbox_recv_irq(int irq, void *data)
{
	struct apple_mbox *mbox = data;
	int drained = 0;

	spin_lock(&mbox->rx_lock);
	if (likely(!READ_ONCE(mbox->removing)))
		drained = apple_mbox_poll_locked(mbox);
	if (mbox->hw->is_ascwrap_v8)
		apple_mbox_v8_rx_irq_guard(mbox, drained);
	spin_unlock(&mbox->rx_lock);

	return IRQ_HANDLED;
}

int apple_mbox_poll(struct apple_mbox *mbox)
{
	unsigned long flags;
	int ret;

	spin_lock_irqsave(&mbox->rx_lock, flags);
	if (unlikely(READ_ONCE(mbox->removing)))
		ret = -ENODEV;
	else
		ret = apple_mbox_poll_locked(mbox);
	spin_unlock_irqrestore(&mbox->rx_lock, flags);

	return ret;
}
EXPORT_SYMBOL(apple_mbox_poll);

/* ASCWrap v8 receive poller: poll-only mode, or after the storm guard. */
static void apple_mbox_v8_rx_poll_work(struct work_struct *work)
{
	struct apple_mbox_priv *priv = container_of(to_delayed_work(work),
						    struct apple_mbox_priv,
						    rx_poll_work);
	struct apple_mbox *mbox = &priv->mbox;

	if (!READ_ONCE(mbox->active) || READ_ONCE(mbox->removing))
		return;
	apple_mbox_poll(mbox);
	queue_delayed_work(system_highpri_wq, &priv->rx_poll_work,
			   APPLE_ASCWRAP_V8_RX_POLL_JIFFIES);
}

/* lifecycle_lock held, mbox->active already set. */
static void apple_mbox_v8_rx_start(struct apple_mbox *mbox)
{
	struct apple_mbox_priv *priv = apple_mbox_to_priv(mbox);
	unsigned long flags;

	spin_lock_irqsave(&mbox->rx_lock, flags);
	priv->rx_irq_empty_run = 0;
	priv->rx_psd_skip_logged = false;
	priv->rx_irq_live = !priv->rx_poll_only;
	spin_unlock_irqrestore(&mbox->rx_lock, flags);

	if (priv->rx_poll_only)
		queue_delayed_work(system_highpri_wq, &priv->rx_poll_work, 0);
	else
		enable_irq(mbox->irq_recv_not_empty);
}

/*
 * lifecycle_lock held (or removal), mbox->active already cleared. The storm
 * guard and this path each claim rx_irq_live under rx_lock, so exactly one
 * of them balances the enable_irq(). The poller re-queues itself, which
 * cancel_delayed_work_sync() handles; it never takes lifecycle_lock.
 */
static void apple_mbox_v8_rx_stop(struct apple_mbox *mbox)
{
	struct apple_mbox_priv *priv = apple_mbox_to_priv(mbox);
	unsigned long flags;
	bool live;

	spin_lock_irqsave(&mbox->rx_lock, flags);
	live = priv->rx_irq_live;
	priv->rx_irq_live = false;
	spin_unlock_irqrestore(&mbox->rx_lock, flags);

	if (live)
		disable_irq(mbox->irq_recv_not_empty);
	cancel_delayed_work_sync(&priv->rx_poll_work);
}

int apple_mbox_start(struct apple_mbox *mbox)
{
	u32 control;
	int ret = 0;

	mutex_lock(&mbox->lifecycle_lock);
	if (mbox->removing) {
		ret = -ENODEV;
		goto out_unlock;
	}
	if (mbox->active)
		goto out_unlock;

	ret = pm_runtime_resume_and_get(mbox->dev);
	if (ret) {
		dev_err(mbox->dev,
			"mailbox start: runtime resume failed: %d (active=%d status=%d disable_depth=%d runtime_error=%d)\n",
			ret, mbox->active, mbox->dev->power.runtime_status,
			mbox->dev->power.disable_depth,
			mbox->dev->power.runtime_error);
		apple_mbox_clear_resume_error(mbox);
		goto out_unlock;
	}

	if (mbox->hw->ap_initializes_mailboxes) {
		/* The primary T8140 outbox requires its enable bit before use. */
		control = readl(mbox->regs + mbox->hw->i2a_control);
		writel(control | BIT(0), mbox->regs + mbox->hw->i2a_control);
	}

	/*
	 * Only some variants of this mailbox HW provide interrupt control
	 * at the mailbox level. We therefore need to handle enabling/disabling
	 * interrupts at the main interrupt controller anyway for hardware that
	 * doesn't. Just always keep the interrupts we care about enabled at
	 * the mailbox level so that both hardware revisions behave almost
	 * the same.
	 */
	if (mbox->hw->has_irq_controls) {
		writel_relaxed(mbox->hw->irq_bit_recv_not_empty |
				       mbox->hw->irq_bit_send_empty,
			       mbox->regs + mbox->hw->irq_enable);
	}

	if (apple_mbox_has_rx_poller(mbox)) {
		WRITE_ONCE(mbox->active, true);
		apple_mbox_v8_rx_start(mbox);
		goto out_unlock;
	}

	enable_irq(mbox->irq_recv_not_empty);
	mbox->active = true;

out_unlock:
	mutex_unlock(&mbox->lifecycle_lock);
	return ret;
}
EXPORT_SYMBOL(apple_mbox_start);

static void apple_mbox_stop_locked(struct apple_mbox *mbox)
{
	if (!mbox->active)
		return;

	if (apple_mbox_has_rx_poller(mbox)) {
		WRITE_ONCE(mbox->active, false);
		apple_mbox_v8_rx_stop(mbox);
	} else {
		mbox->active = false;
		disable_irq(mbox->irq_recv_not_empty);
	}
	pm_runtime_mark_last_busy(mbox->dev);
	pm_runtime_put_autosuspend(mbox->dev);
}

void apple_mbox_stop(struct apple_mbox *mbox)
{
	mutex_lock(&mbox->lifecycle_lock);
	apple_mbox_stop_locked(mbox);
	mutex_unlock(&mbox->lifecycle_lock);
}
EXPORT_SYMBOL(apple_mbox_stop);

/*
 * Stop the provider-owned CPU, then quiesce every transport edge before
 * devres releases IRQs or MMIO mappings. Device links order consumer removal
 * first; the callback clear and waiter wake also cover future consumers that
 * violate that ownership rule.
 */
static void apple_mbox_quiesce(void *data)
{
	struct apple_mbox *mbox = data;
	unsigned long flags;
	bool had_rx;
	bool was_active;

	mutex_lock(&mbox->lifecycle_lock);
	if (mbox->removing) {
		mutex_unlock(&mbox->lifecycle_lock);
		return;
	}
	apple_mbox_ascwrap_v6_stop_cpu_locked(mbox);

	/* Serialize the removal fence with every MMIO send critical section. */
	spin_lock_irqsave(&mbox->tx_lock, flags);
	WRITE_ONCE(mbox->removing, true);
	spin_unlock_irqrestore(&mbox->tx_lock, flags);

	/*
	 * Publish the inactive state under the lifecycle lock, then drop it
	 * before disable_irq() waits for a callback. A callback is allowed to
	 * finish a send or stop request without an rx_lock -> lifecycle_lock
	 * inversion. The removal fence prevents any matching restart.
	 */
	was_active = mbox->active;
	WRITE_ONCE(mbox->active, false);
	mutex_unlock(&mbox->lifecycle_lock);
	if (was_active) {
		if (apple_mbox_has_rx_poller(mbox))
			apple_mbox_v8_rx_stop(mbox);
		else
			disable_irq(mbox->irq_recv_not_empty);
		pm_runtime_mark_last_busy(mbox->dev);
		pm_runtime_put_autosuspend(mbox->dev);
	}
	/* The v8 poller also stops on the removal fence; make sure it is gone. */
	if (apple_mbox_has_rx_poller(mbox))
		cancel_delayed_work_sync(&apple_mbox_to_priv(mbox)->rx_poll_work);

	/*
	 * Polled providers claim no IRQs, and the send-empty line is optional:
	 * drain only the handlers that exist.
	 */
	if (mbox->irq_send_empty >= 0) {
		/* Serialize once-only masking with the IRQ and send-error paths. */
		spin_lock_irqsave(&mbox->tx_lock, flags);
		if (mbox->tx_irq_unmasked) {
			disable_irq_nosync(mbox->irq_send_empty);
			mbox->tx_irq_unmasked = false;
		}
		spin_unlock_irqrestore(&mbox->tx_lock, flags);
		/* The handler takes tx_lock, so drain it after releasing the lock. */
		synchronize_irq(mbox->irq_send_empty);
	}
	complete_all(&mbox->tx_empty);
	if (mbox->irq_recv_not_empty >= 0)
		synchronize_irq(mbox->irq_recv_not_empty);

	spin_lock_irqsave(&mbox->rx_lock, flags);
	had_rx = mbox->rx;
	mbox->rx = NULL;
	mbox->cookie = NULL;
	spin_unlock_irqrestore(&mbox->rx_lock, flags);

	if (WARN_ON_ONCE(had_rx))
		dev_warn(mbox->dev,
			 "consumer callback survived provider remove ordering\n");

	pm_runtime_barrier(mbox->dev);
}

struct apple_mbox *apple_mbox_get(struct device *dev, int index)
{
	struct of_phandle_args args;
	struct platform_device *pdev;
	struct apple_mbox *mbox;
	int ret;

	ret = of_parse_phandle_with_args(dev->of_node, "mboxes", "#mbox-cells",
					 index, &args);
	if (ret || !args.np)
		return ERR_PTR(ret);

	pdev = of_find_device_by_node(args.np);
	of_node_put(args.np);

	if (!pdev)
		return ERR_PTR(-EPROBE_DEFER);

	mbox = platform_get_drvdata(pdev);
	if (!mbox) {
		mbox = ERR_PTR(-EPROBE_DEFER);
		goto out_put_pdev;
	}

	if (!device_link_add(dev, &pdev->dev, DL_FLAG_AUTOREMOVE_CONSUMER)) {
		mbox = ERR_PTR(-ENODEV);
		goto out_put_pdev;
	}

out_put_pdev:
	put_device(&pdev->dev);

	return mbox;
}
EXPORT_SYMBOL(apple_mbox_get);

struct apple_mbox *apple_mbox_get_byname(struct device *dev, const char *name)
{
	int index;

	index = of_property_match_string(dev->of_node, "mbox-names", name);
	if (index < 0)
		return ERR_PTR(index);

	return apple_mbox_get(dev, index);
}
EXPORT_SYMBOL(apple_mbox_get_byname);

static void apple_mbox_iounmap(void *addr)
{
	iounmap((__force void __iomem *)addr);
}

/*
 * Validate and map one T8152 GPU ASCWrap v8 provider. Nothing here reads or
 * writes device registers: the four windows are only mapped. The wrapper,
 * iop-vbar and post-office window are requested exclusively; the PSD block
 * is mapped without a request so that a GPU power owner may also map the
 * same service block (the provider itself only ever reads it).
 */
static int apple_mbox_probe_ascwrap_v8(struct platform_device *pdev,
				       struct apple_mbox *mbox)
{
	struct device *dev = &pdev->dev;
	struct apple_mbox_priv *priv = apple_mbox_to_priv(mbox);
	struct resource *wrapper, *iop_vbar, *window, *psd;
	u64 wrapper_base, iop_vbar_base, window_base, psd_base;
	const char *firmware_role;
	int ret;

	wrapper = platform_get_resource_byname(pdev, IORESOURCE_MEM, "wrapper");
	iop_vbar = platform_get_resource_byname(pdev, IORESOURCE_MEM, "iop-vbar");
	window = platform_get_resource_byname(pdev, IORESOURCE_MEM, "mailbox");
	psd = platform_get_resource_byname(pdev, IORESOURCE_MEM, "psd");
	if (!wrapper || !iop_vbar || !window || !psd ||
	    resource_size(wrapper) != APPLE_ASCWRAP_V8_WRAPPER_SIZE ||
	    resource_size(iop_vbar) != APPLE_ASCWRAP_V8_IOP_VBAR_SIZE ||
	    resource_size(window) != APPLE_ASCWRAP_V8_MBOX_SIZE ||
	    resource_size(psd) != APPLE_ASCWRAP_V8_PSD_SIZE)
		return dev_err_probe(dev, -EINVAL,
				     "invalid ASCWrap v8 provider resources\n");

	ret = of_property_read_string(dev->of_node, "apple,firmware-role",
				      &firmware_role);
	if (ret)
		return dev_err_probe(dev, ret,
				     "missing ASCWrap v8 firmware role\n");

	if (!strcmp(firmware_role, "GFX")) {
		mbox->ascwrap_v6.role = APPLE_MBOX_ASCWRAP_V6_ROLE_GFX;
		wrapper_base = APPLE_T8152_GFX_WRAPPER_BASE;
		iop_vbar_base = APPLE_T8152_GFX_IOP_VBAR_BASE;
		window_base = APPLE_T8152_GFX_MBOX_BASE;
		psd_base = APPLE_T8152_GFX_PSD_BASE;
	} else if (!strcmp(firmware_role, "GFX1")) {
		mbox->ascwrap_v6.role = APPLE_MBOX_ASCWRAP_V6_ROLE_GFX1;
		wrapper_base = APPLE_T8152_GFX1_WRAPPER_BASE;
		iop_vbar_base = APPLE_T8152_GFX1_IOP_VBAR_BASE;
		window_base = APPLE_T8152_GFX1_MBOX_BASE;
		psd_base = APPLE_T8152_GFX1_PSD_BASE;
	} else {
		return dev_err_probe(dev, -EINVAL,
				     "invalid ASCWrap v8 firmware role\n");
	}

	/*
	 * Fixed per-SoC CPU-physical windows. In particular the post-office
	 * window must be exactly this role's 0x80 bytes: the rest of the bank
	 * belongs to PMP, DCP, DCPEXT and SCALER0.
	 */
	if (wrapper->start != wrapper_base || iop_vbar->start != iop_vbar_base ||
	    window->start != window_base || psd->start != psd_base)
		return dev_err_probe(dev, -EINVAL,
				     "%s ASCWrap v8 resources do not match the T8152 profile\n",
				     firmware_role);

	mbox->ascwrap_v6.wrapper_start = wrapper->start;
	mbox->ascwrap_v6.wrapper_size = resource_size(wrapper);
	mbox->ascwrap_v6.iop_vbar_start = iop_vbar->start;
	mbox->ascwrap_v6.iop_vbar_size = resource_size(iop_vbar);
	mbox->ascwrap_v6.missing = ascwrap_v8_cpu_control ?
		0 : APPLE_ASCWRAP_V8_MISSING_CPU_CONTROL_APPROVAL;
	priv->rx_poll_only = !ascwrap_v8_rx_irq;
	INIT_DELAYED_WORK(&priv->rx_poll_work, apple_mbox_v8_rx_poll_work);

	mbox->wrapper_regs = devm_platform_ioremap_resource_byname(pdev, "wrapper");
	if (IS_ERR(mbox->wrapper_regs))
		return PTR_ERR(mbox->wrapper_regs);

	mbox->iop_vbar_regs =
		devm_platform_ioremap_resource_byname(pdev, "iop-vbar");
	if (IS_ERR(mbox->iop_vbar_regs))
		return PTR_ERR(mbox->iop_vbar_regs);

	mbox->regs = devm_platform_ioremap_resource_byname(pdev, "mailbox");
	if (IS_ERR(mbox->regs))
		return PTR_ERR(mbox->regs);

	/*
	 * Same posted/non-posted choice devm_ioremap_resource() makes, but
	 * without claiming the region. Released after apple_mbox_quiesce(),
	 * which is registered later and may still read the PSD state.
	 */
	if (psd->flags & IORESOURCE_MEM_NONPOSTED)
		priv->psd_regs = ioremap_np(psd->start, resource_size(psd));
	else
		priv->psd_regs = ioremap(psd->start, resource_size(psd));
	if (!priv->psd_regs)
		return -ENOMEM;
	ret = devm_add_action_or_reset(dev, apple_mbox_iounmap,
				       (__force void *)priv->psd_regs);
	if (ret)
		return ret;

	dev_info(dev,
		 "%s ASCWrap v8 provider: wrapper %pR, post-office window %pR, PSD %pR; CPU control %s; receive %s\n",
		 firmware_role, wrapper, window, psd,
		 ascwrap_v8_cpu_control ? "APPROVED" : "refused (default)",
		 ascwrap_v8_rx_irq ? "recv-not-empty IRQ (APPROVED)" :
				     "polled, no IRQ enabled (default)");
	return 0;
}

static int apple_mbox_probe(struct platform_device *pdev)
{
	int ret;
	char *irqname;
	const char *firmware_role;
	struct apple_mbox_priv *priv;
	struct apple_mbox *mbox;
	struct device *dev = &pdev->dev;
	struct resource *res, *iop_vbar_res;

	priv = devm_kzalloc(dev, sizeof(*priv), GFP_KERNEL);
	if (!priv)
		return -ENOMEM;
	mbox = &priv->mbox;

	mbox->dev = &pdev->dev;
	mbox->hw = of_device_get_match_data(dev);
	if (!mbox->hw)
		return -EINVAL;
	if (mbox->hw->counted_fifo && !IS_ENABLED(CONFIG_ARM64))
		return -EOPNOTSUPP;

	if (mbox->hw->is_ascwrap_v6) {
		/*
		 * Validate the entire provider identity before mapping either
		 * resource. These are fixed per-SoC CPU-physical windows and role
		 * assignments, not generic consumer-provided aliases.
		 */
		res = platform_get_resource_byname(pdev, IORESOURCE_MEM, "wrapper");
		iop_vbar_res = platform_get_resource_byname(pdev, IORESOURCE_MEM,
							    "iop-vbar");
		if (!res || !iop_vbar_res ||
		    resource_size(res) != APPLE_ASCWRAP_V6_WRAPPER_SIZE ||
		    resource_size(iop_vbar_res) != APPLE_ASCWRAP_V6_IOP_VBAR_SIZE)
			return dev_err_probe(dev, -EINVAL,
					     "invalid ASCWrap v6 provider resources\n");

		ret = of_property_read_string(dev->of_node, "apple,firmware-role",
					      &firmware_role);
		if (ret)
			return dev_err_probe(dev, ret,
					     "missing ASCWrap v6 firmware role\n");

		if (!strcmp(firmware_role, "GFX")) {
			mbox->ascwrap_v6.role = APPLE_MBOX_ASCWRAP_V6_ROLE_GFX;
			if (res->start != APPLE_T8140_GFX_WRAPPER_BASE +
					  mbox->hw->ascwrap_v6_wrapper_delta ||
			    iop_vbar_res->start != APPLE_T8140_GFX_IOP_VBAR_BASE +
						 mbox->hw->ascwrap_v6_base_delta)
				return dev_err_probe(dev, -EINVAL,
						     "GFX ASCWrap v6 resources do not match the SoC profile\n");
		} else if (!strcmp(firmware_role, "GFX1")) {
			mbox->ascwrap_v6.role = APPLE_MBOX_ASCWRAP_V6_ROLE_GFX1;
			if (res->start != APPLE_T8140_GFX1_WRAPPER_BASE +
					  mbox->hw->ascwrap_v6_wrapper_delta ||
			    iop_vbar_res->start != APPLE_T8140_GFX1_IOP_VBAR_BASE +
						 mbox->hw->ascwrap_v6_base_delta)
				return dev_err_probe(dev, -EINVAL,
						     "GFX1 ASCWrap v6 resources do not match the SoC profile\n");
		} else {
			return dev_err_probe(dev, -EINVAL,
					     "invalid ASCWrap v6 firmware role\n");
		}

		mbox->ascwrap_v6.wrapper_start = res->start;
		mbox->ascwrap_v6.wrapper_size = resource_size(res);
		mbox->ascwrap_v6.iop_vbar_start = iop_vbar_res->start;
		mbox->ascwrap_v6.iop_vbar_size = resource_size(iop_vbar_res);
		/* resetState() is software-only; the matching stop lives above. */
		mbox->ascwrap_v6.missing = 0;

		mbox->wrapper_regs =
			devm_platform_ioremap_resource_byname(pdev, "wrapper");
		if (IS_ERR(mbox->wrapper_regs))
			return PTR_ERR(mbox->wrapper_regs);

		mbox->iop_vbar_regs =
			devm_platform_ioremap_resource_byname(pdev, "iop-vbar");
		if (IS_ERR(mbox->iop_vbar_regs))
			return PTR_ERR(mbox->iop_vbar_regs);

		mbox->regs = (u8 __iomem *)mbox->wrapper_regs +
			     mbox->hw->reg_offset;
	} else if (mbox->hw->is_ascwrap_v8) {
		ret = apple_mbox_probe_ascwrap_v8(pdev, mbox);
		if (ret)
			return ret;
	} else {
		res = platform_get_resource(pdev, IORESOURCE_MEM, 0);
		if (mbox->hw->rx_poll_required &&
		    (!of_machine_is_compatible("apple,j873g") || !res ||
		     res->start != mbox->hw->j873_window || resource_size(res) != 0x80))
			return -EINVAL;
		if (!res || resource_size(res) < mbox->hw->reg_offset +
			    max(mbox->hw->i2a_recv1 + sizeof(u64),
				mbox->hw->i2a_control + sizeof(u32)))
			return dev_err_probe(dev, -EINVAL,
					     "mailbox register window is too small\n");

		mbox->regs = devm_platform_ioremap_resource(pdev, 0);
		if (IS_ERR(mbox->regs))
			return PTR_ERR(mbox->regs);
		mbox->regs = (u8 __iomem *)mbox->regs + mbox->hw->reg_offset;
	}

	spin_lock_init(&mbox->rx_lock);
	spin_lock_init(&mbox->tx_lock);
	mutex_init(&mbox->lifecycle_lock);
	init_completion(&mbox->tx_empty);
	if (mbox->hw->rx_poll_required) {
		priv->rx_poll_only = true;
		INIT_DELAYED_WORK(&priv->rx_poll_work, apple_mbox_v8_rx_poll_work);
		mbox->irq_recv_not_empty = mbox->irq_send_empty = -1;
		dev_info(dev, "J873 postoffice: paired transfers, bounded polling, no IRQ claim\n");
		goto setup_pm;
	}

	mbox->irq_recv_not_empty =
		platform_get_irq_byname(pdev, "recv-not-empty");
	if (mbox->irq_recv_not_empty < 0)
		return mbox->irq_recv_not_empty;

	/*
	 * Some coprocessors (e.g. the T6021 ANE ASC) have no send-empty line.
	 * apple_mbox_send then polls the A2I control register instead.
	 */
	mbox->irq_send_empty =
		platform_get_irq_byname_optional(pdev, "send-empty");
	if (mbox->irq_send_empty < 0 && mbox->irq_send_empty != -ENXIO)
		return mbox->irq_send_empty;

	irqname = devm_kasprintf(dev, GFP_KERNEL, "%s-recv", dev_name(dev));
	if (!irqname)
		return -ENOMEM;

	ret = devm_request_irq(dev, mbox->irq_recv_not_empty,
			       apple_mbox_recv_irq,
			       IRQF_NO_AUTOEN | IRQF_NO_SUSPEND, irqname, mbox);
	if (ret)
		return ret;

	if (mbox->irq_send_empty >= 0) {
		irqname = devm_kasprintf(dev, GFP_KERNEL, "%s-send",
					 dev_name(dev));
		if (!irqname)
			return -ENOMEM;

		ret = devm_request_irq(dev, mbox->irq_send_empty,
				       apple_mbox_send_empty_irq,
				       IRQF_NO_AUTOEN | IRQF_NO_SUSPEND,
				       irqname, mbox);
		if (ret)
			return ret;
	}

setup_pm:
	ret = devm_pm_runtime_enable(dev);
	if (ret)
		return ret;
	ret = devm_add_action_or_reset(dev, apple_mbox_quiesce, mbox);
	if (ret)
		return ret;

	platform_set_drvdata(pdev, mbox);
	return 0;
}

static void apple_mbox_remove(struct platform_device *pdev)
{
	struct apple_mbox *mbox = platform_get_drvdata(pdev);

	/* Block new phandle lookups before quiescing the provider. */
	platform_set_drvdata(pdev, NULL);
	apple_mbox_quiesce(mbox);
}

static const struct apple_mbox_hw apple_mbox_t8015_hw = {
	.control_full = APPLE_ASC_MBOX_CONTROL_FULL,
	.control_empty = APPLE_ASC_MBOX_CONTROL_EMPTY,

	.a2i_control = APPLE_T8015_MBOX_A2I_CONTROL,
	.a2i_send0 = APPLE_ASC_MBOX_A2I_SEND0,
	.a2i_send1 = APPLE_ASC_MBOX_A2I_SEND1,

	.i2a_control = APPLE_T8015_MBOX_I2A_CONTROL,
	.i2a_recv0 = APPLE_ASC_MBOX_I2A_RECV0,
	.i2a_recv1 = APPLE_ASC_MBOX_I2A_RECV1,

	.has_irq_controls = false,
};

static const struct apple_mbox_hw apple_mbox_asc_hw = {
	.control_full = APPLE_ASC_MBOX_CONTROL_FULL,
	.control_empty = APPLE_ASC_MBOX_CONTROL_EMPTY,

	.a2i_control = APPLE_ASC_MBOX_A2I_CONTROL,
	.a2i_send0 = APPLE_ASC_MBOX_A2I_SEND0,
	.a2i_send1 = APPLE_ASC_MBOX_A2I_SEND1,

	.i2a_control = APPLE_ASC_MBOX_I2A_CONTROL,
	.i2a_recv0 = APPLE_ASC_MBOX_I2A_RECV0,
	.i2a_recv1 = APPLE_ASC_MBOX_I2A_RECV1,

	.has_irq_controls = false,
};

static const struct apple_mbox_hw apple_mbox_t8140_asc_hw = {
	.ap_initializes_mailboxes = true,

	.control_full = APPLE_ASC_MBOX_CONTROL_FULL,
	.control_empty = APPLE_ASC_MBOX_CONTROL_EMPTY,

	.a2i_control = APPLE_ASC_MBOX_A2I_CONTROL,
	.a2i_send0 = APPLE_ASC_MBOX_A2I_SEND0,
	.a2i_send1 = APPLE_ASC_MBOX_A2I_SEND1,

	.i2a_control = APPLE_ASC_MBOX_I2A_CONTROL,
	.i2a_recv0 = APPLE_ASC_MBOX_I2A_RECV0,
	.i2a_recv1 = APPLE_ASC_MBOX_I2A_RECV1,

	.has_irq_controls = false,
};

static const struct apple_mbox_hw apple_mbox_ascwrap_v6_hw = {
	.reg_offset = APPLE_ASCWRAP_V6_MBOX_OFFSET,
	.is_ascwrap_v6 = true,

	.control_full = APPLE_ASC_MBOX_CONTROL_FULL,
	.control_empty = APPLE_ASC_MBOX_CONTROL_EMPTY,

	.a2i_control = APPLE_ASC_MBOX_A2I_CONTROL,
	.a2i_send0 = APPLE_ASC_MBOX_A2I_SEND0,
	.a2i_send1 = APPLE_ASC_MBOX_A2I_SEND1,

	.i2a_control = APPLE_ASC_MBOX_I2A_CONTROL,
	.i2a_recv0 = APPLE_ASC_MBOX_I2A_RECV0,
	.i2a_recv1 = APPLE_ASC_MBOX_I2A_RECV1,

	.has_irq_controls = false,
};

/* J813/25G76 AppleASCWrapV6::_runCPU at 0xfffffe0008a42d34:
 * start reads +0x44 and sets bit4; stop clears bit4, rereads, then clears
 * bit5. This matches the provider transition above. The four ADT resource
 * bases move from T8140's 0x4... bank to T8142's 0x5... bank. Keep this an
 * explicit compatible; selecting it does not admit a GPU firmware ABI.
 */
static const struct apple_mbox_hw apple_mbox_t8142_ascwrap_v6_hw = {
	.reg_offset = APPLE_ASCWRAP_V6_MBOX_OFFSET,
	.is_ascwrap_v6 = true,
	.ascwrap_v6_base_delta = APPLE_T8142_ASCWRAP_BASE_DELTA,
	.ascwrap_v6_wrapper_delta = APPLE_T8142_ASCWRAP_BASE_DELTA,
	.control_full = APPLE_ASC_MBOX_CONTROL_FULL,
	.control_empty = APPLE_ASC_MBOX_CONTROL_EMPTY,
	.a2i_control = APPLE_ASC_MBOX_A2I_CONTROL,
	.a2i_send0 = APPLE_ASC_MBOX_A2I_SEND0,
	.a2i_send1 = APPLE_ASC_MBOX_A2I_SEND1,
	.i2a_control = APPLE_ASC_MBOX_I2A_CONTROL,
	.i2a_recv0 = APPLE_ASC_MBOX_I2A_RECV0,
	.i2a_recv1 = APPLE_ASC_MBOX_I2A_RECV1,
	.has_irq_controls = false,
};

static const struct apple_mbox_hw apple_mbox_t8152_v8_hw = {
	.counted_fifo = true,
	.control_full = GENMASK(11, 8), /* free TX slots */
	.control_empty = GENMASK(3, 0), /* RX occupancy */
	.a2i_control = 0x20,
	.a2i_send0 = 0x10,
	.a2i_send1 = 0x18,
	.i2a_control = 0x20,
	.i2a_recv0 = 0x00,
	.i2a_recv1 = 0x08,
	.has_irq_controls = false,
};

static const struct apple_mbox_hw apple_mbox_t8152_pmp_hw = {
	.rx_poll_required = true,
	.j873_window = 0x316020000ULL,
	.counted_fifo = true,
	.control_full = GENMASK(11, 8),
	.control_empty = GENMASK(3, 0),
	.a2i_control = 0x20,
	.a2i_send0 = 0x10,
	.a2i_send1 = 0x18,
	.i2a_control = 0x20,
	.i2a_recv0 = 0x00,
	.i2a_recv1 = 0x08,
	.has_irq_controls = false,
};

/* ANS: msgbox-mailbox-num 3 in the non-alias post-office bank. */
static const struct apple_mbox_hw apple_mbox_t8152_ans_hw = {
	.rx_poll_required = true,
	.j873_window = 0x316010300ULL,
	.counted_fifo = true,
	.control_full = GENMASK(11, 8),
	.control_empty = GENMASK(3, 0),
	.a2i_control = 0x20,
	.a2i_send0 = 0x10,
	.a2i_send1 = 0x18,
	.i2a_control = 0x20,
	.i2a_recv0 = 0x00,
	.i2a_recv1 = 0x08,
	.has_irq_controls = false,
};

/* T8152/J873 GPU GFX/GFX1 ASCWrap v8: the counted-FIFO layout of
 * apple_mbox_t8152_v8_hw at the role's post-office window (reg "mailbox"),
 * plus wrapper lifecycle reads gated on the role's PSD service. The SMC's
 * apple,t8152-ascwrap-v8-mailbox entry above is unchanged.
 */
static const struct apple_mbox_hw apple_mbox_t8152_gfx_ascwrap_v8_hw = {
	.is_ascwrap_v8 = true,
	.counted_fifo = true,
	.control_full = GENMASK(11, 8), /* free TX slots */
	.control_empty = GENMASK(3, 0), /* RX occupancy */
	.a2i_control = 0x20,
	.a2i_send0 = 0x10,
	.a2i_send1 = 0x18,
	.i2a_control = 0x20,
	.i2a_recv0 = 0x00,
	.i2a_recv1 = 0x08,
	.has_irq_controls = false,
};

static_assert(0x20 + sizeof(u32) <= APPLE_ASCWRAP_V8_MBOX_SIZE);

static const struct apple_mbox_hw apple_mbox_t8140_aop_setup_hw = {
	.control_full = APPLE_ASC_MBOX_CONTROL_FULL,
	.control_empty = APPLE_ASC_MBOX_CONTROL_EMPTY,

	.a2i_control = APPLE_T8140_AOP_SETUP_A2I_CONTROL,
	.a2i_send0 = APPLE_T8140_AOP_SETUP_A2I_SEND0,
	.a2i_send1 = APPLE_T8140_AOP_SETUP_A2I_SEND1,

	.i2a_control = APPLE_T8140_AOP_SETUP_I2A_CONTROL,
	.i2a_recv0 = APPLE_T8140_AOP_SETUP_I2A_RECV0,
	.i2a_recv1 = APPLE_T8140_AOP_SETUP_I2A_RECV1,

	.has_irq_controls = false,
};

static const struct apple_mbox_hw apple_mbox_m3_hw = {
	.control_full = APPLE_M3_MBOX_CONTROL_FULL,
	.control_empty = APPLE_M3_MBOX_CONTROL_EMPTY,

	.a2i_control = APPLE_M3_MBOX_A2I_CONTROL,
	.a2i_send0 = APPLE_M3_MBOX_A2I_SEND0,
	.a2i_send1 = APPLE_M3_MBOX_A2I_SEND1,

	.i2a_control = APPLE_M3_MBOX_I2A_CONTROL,
	.i2a_recv0 = APPLE_M3_MBOX_I2A_RECV0,
	.i2a_recv1 = APPLE_M3_MBOX_I2A_RECV1,

	.has_irq_controls = true,
	.irq_enable = APPLE_M3_MBOX_IRQ_ENABLE,
	.irq_ack = APPLE_M3_MBOX_IRQ_ACK,
	.irq_bit_recv_not_empty = APPLE_M3_MBOX_IRQ_I2A_NOT_EMPTY,
	.irq_bit_send_empty = APPLE_M3_MBOX_IRQ_A2I_EMPTY,
};

/* T6050's native ASCWrap v6 uses the same RUN/stop sequence. Aura grants
 * these complete wrapper/VBAR windows in the retained GPU aperture. */
static const struct apple_mbox_hw apple_mbox_t6050_ascwrap_v6_hw = {
	.reg_offset = APPLE_ASCWRAP_V6_MBOX_OFFSET,
	.is_ascwrap_v6 = true,
	.ascwrap_v6_base_delta = APPLE_T6050_ASCWRAP_BASE_DELTA,
	.ascwrap_v6_wrapper_delta = APPLE_T6050_ASCWRAP_BASE_DELTA,
	.control_full = APPLE_ASC_MBOX_CONTROL_FULL,
	.control_empty = APPLE_ASC_MBOX_CONTROL_EMPTY,
	.a2i_control = APPLE_ASC_MBOX_A2I_CONTROL,
	.a2i_send0 = APPLE_ASC_MBOX_A2I_SEND0,
	.a2i_send1 = APPLE_ASC_MBOX_A2I_SEND1,
	.i2a_control = APPLE_ASC_MBOX_I2A_CONTROL,
	.i2a_recv0 = APPLE_ASC_MBOX_I2A_RECV0,
	.i2a_recv1 = APPLE_ASC_MBOX_I2A_RECV1,
	.has_irq_controls = false,
};

/* Retained compatibility profile: unshifted wrapper aliases, native VBAR. */
static const struct apple_mbox_hw apple_mbox_t6050_apif_ascwrap_v6_hw = {
	.reg_offset = APPLE_ASCWRAP_V6_MBOX_OFFSET,
	.is_ascwrap_v6 = true,
	.ascwrap_v6_base_delta = APPLE_T6050_ASCWRAP_BASE_DELTA,
	.control_full = APPLE_ASC_MBOX_CONTROL_FULL,
	.control_empty = APPLE_ASC_MBOX_CONTROL_EMPTY,
	.a2i_control = APPLE_ASC_MBOX_A2I_CONTROL,
	.a2i_send0 = APPLE_ASC_MBOX_A2I_SEND0,
	.a2i_send1 = APPLE_ASC_MBOX_A2I_SEND1,
	.i2a_control = APPLE_ASC_MBOX_I2A_CONTROL,
	.i2a_recv0 = APPLE_ASC_MBOX_I2A_RECV0,
	.i2a_recv1 = APPLE_ASC_MBOX_I2A_RECV1,
	.has_irq_controls = false,
};

static const struct of_device_id apple_mbox_of_match[] = {
	{ .compatible = "apple,t8152-pmp-mailbox", .data = &apple_mbox_t8152_pmp_hw },
	{ .compatible = "apple,t8152-ans-mailbox", .data = &apple_mbox_t8152_ans_hw },
	{ .compatible = "apple,t8152-ascwrap-v8-mailbox", .data = &apple_mbox_t8152_v8_hw },
	{ .compatible = "apple,t8152-gfx-ascwrap-v8", .data = &apple_mbox_t8152_gfx_ascwrap_v8_hw },
	{ .compatible = "apple,t8140-ascwrap-v6", .data = &apple_mbox_ascwrap_v6_hw },
	{ .compatible = "apple,t8142-ascwrap-v6", .data = &apple_mbox_t8142_ascwrap_v6_hw },
	{ .compatible = "apple,t6050-apif-ascwrap-v6", .data = &apple_mbox_t6050_apif_ascwrap_v6_hw },
	{ .compatible = "apple,t6050-ascwrap-v6", .data = &apple_mbox_t6050_ascwrap_v6_hw },
	{ .compatible = "apple,t6050-x1n1-ascwrap-v6", .data = &apple_mbox_t6050_ascwrap_v6_hw },
	{ .compatible = "apple,t8140-asc-mailbox", .data = &apple_mbox_t8140_asc_hw },
	{ .compatible = "apple,t8140-aop-setup-mailbox", .data = &apple_mbox_t8140_aop_setup_hw },
	{ .compatible = "apple,asc-mailbox-v4", .data = &apple_mbox_asc_hw },
	{ .compatible = "apple,t8015-asc-mailbox", .data = &apple_mbox_t8015_hw },
	{ .compatible = "apple,m3-mailbox-v2", .data = &apple_mbox_m3_hw },
	{}
};
MODULE_DEVICE_TABLE(of, apple_mbox_of_match);

static struct platform_driver apple_mbox_driver = {
	.driver = {
		.name = "apple-mailbox",
		.of_match_table = apple_mbox_of_match,
	},
	.probe = apple_mbox_probe,
	.remove = apple_mbox_remove,
};
module_platform_driver(apple_mbox_driver);

MODULE_LICENSE("Dual MIT/GPL");
MODULE_AUTHOR("Sven Peter <sven@svenpeter.dev>");
MODULE_DESCRIPTION("Apple Mailbox driver");
