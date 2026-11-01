// SPDX-License-Identifier: GPL-2.0-only OR MIT
/* Copyright 2023 */

#include <linux/completion.h>
#include <linux/of.h>

#include "afk.h"
#include "dcp.h"

static void disp_service_init(struct apple_epic_service *service, const char *name,
			const char *class, s64 unit)
{
}


static const struct apple_epic_service_ops ibootep_ops[] = {
	{
		.name = "disp0-service",
		.init = disp_service_init,
	},
	{}
};

/*
 * The service name follows the pipe, not the endpoint.  The internal DCP
 * offers "disp0-service" on DISP0_ENDPOINT; the J700 (T8140) external one
 * offers "dispext-service" on the same endpoint, and an unmatched service is
 * only reported as an AFK error:
 *
 *   AFK[ep:23]: unable to match service dispext-service on channel 1
 *
 * which is easy to read as noise from an endpoint that should not have been
 * started.  Both are the same do-nothing handshake, so accept both on the
 * T8140 DCPs (the external node also lists "apple,t8140-dcp").  Other SoCs'
 * DCPs keep the disp0-service table.
 */
static const struct apple_epic_service_ops ibootep_t8140_ops[] = {
	{
		.name = "disp0-service",
		.init = disp_service_init,
	},
	{
		.name = "dispext-service",
		.init = disp_service_init,
	},
	{}
};

int ibootep_init(struct apple_dcp *dcp)
{
	const struct apple_epic_service_ops *ops = ibootep_ops;

	if (of_device_is_compatible(dcp->dev->of_node, "apple,t8140-dcp"))
		ops = ibootep_t8140_ops;

	dcp->ibootep = afk_init(dcp, DISP0_ENDPOINT, ops);
	afk_start(dcp->ibootep);

	return 0;
}
