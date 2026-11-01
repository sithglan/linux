// SPDX-License-Identifier: GPL-2.0-only OR MIT
/* Copyright 2022 Sven Peter <sven@svenpeter.dev> */

#include <linux/bitfield.h>
#include <linux/completion.h>
#include <linux/module.h>
#include <linux/phy/phy.h>
#include <linux/delay.h>
#include <linux/jiffies.h>
#include <linux/printk.h>

#include <linux/soc/apple/dp-tunnel.h>

#include "afk.h"
#include "dcp.h"
#include "dptxep.h"
#include "parser.h"
#include "trace.h"

struct dcpdptx_connection_cmd {
	__le32 unk;
	__le32 target;
} __attribute__((packed));

struct dcpdptx_hotplug_cmd {
	u8 _pad0[16];
	__le32 unk;
} __attribute__((packed));

struct dptxport_apcall_link_rate {
	__le32 retcode;
	u8 _unk0[12];
	__le32 link_rate;
	u8 _unk1[12];
} __attribute__((packed));

struct dptxport_apcall_lane_count {
	__le32 retcode;
	u8 _unk0[12];
	__le64 lane_count;
	u8 _unk1[8];
} __attribute__((packed));

struct dptxport_apcall_set_active_lane_count {
	__le32 retcode;
	u8 _unk0[12];
	__le64 lane_count;
	u8 _unk1[8];
} __packed;

struct dptxport_apcall_get_support {
	__le32 retcode;
	u8 _unk0[12];
	__le32 supported;
	u8 _unk1[12];
} __attribute__((packed));

struct dptxport_apcall_max_drive_settings {
	__le32 retcode;
	u8 _unk0[12];
	__le32 max_drive_settings[2];
	u8 _unk1[8];
};

struct dptxport_apcall_drive_settings {
	__le32 retcode;
	u8 _unk0[12];
	__le32 unk1;
	__le32 unk2;
	__le32 unk3;
	__le32 unk4;
	__le32 unk5;
	__le32 unk6;
	__le32 unk7;
};

struct dptxport_apcall_set_tiled {
	__le32 retcode;
};

#include "dptx-drive-v27.inc"

/*
 * Ported from aurora-silicon/linux#8: a Thunderbolt DP tunnel uses the same
 * plain CORE|ATC|DIE|CONNECTED target as a direct alt-mode PHY. CORE is the
 * DFP port (0 = dpphy, 1/2 = dpin0/dpin1, dcp->dptx_dfp_port), ATC is the
 * route's own ATC index -- no separate DPIN field.
 */
static u32 dptxport_remote_target(struct apple_dcp *dcp, u8 core, u8 atc,
				  u8 die)
{
	return FIELD_PREP(DCPDPTX_REMOTE_PORT_CORE, core) |
	       FIELD_PREP(DCPDPTX_REMOTE_PORT_ATC, atc) |
	       FIELD_PREP(DCPDPTX_REMOTE_PORT_DIE, die) |
	       DCPDPTX_REMOTE_PORT_CONNECTED;
}

int dptxport_validate_connection(struct apple_epic_service *service, u8 core,
				 u8 atc, u8 die)
{
	struct dptx_port *dptx = service->cookie;
	struct dcpdptx_connection_cmd cmd, resp;
	int ret;
	u32 target = dptxport_remote_target(service->ep->dcp, core, atc, die);

	/*
	 * attributes: role (0 = direct PHY, 1 = Thunderbolt/USB4 DP IN) |
	 * supportsHPD << 8. A real, hardware-validated reference
	 * implementation of the equivalent Thunderbolt DP tunnel mechanism
	 * on a different SoC (aurora-silicon/linux#8) adds this exact role
	 * bit for its tunnel routes; our own analog-DPIN path has never set
	 * it, always sending a plain direct-PHY attributes value even
	 * though this is a genuinely USB4-tunneled connection. DCP firmware
	 * appears to treat a failed AUX probe as fatal for role=0 but keeps
	 * training regardless for role=1 (a Thunderbolt tunnel's AUX
	 * proxying may not respond as fast as a direct PHY's): repeated
	 * test runs with substantially different Activate/crossbar ordering
	 * all reached the identical INACTIVE_SINK_DETECTED/DPRX-timeout
	 * outcome, which points at a signal we are not sending at all
	 * rather than a timing issue fixable by reordering.
	 */
	u32 attrs = 0x100 | (dcp_is_usb4_output(service->ep->dcp) ? 1 : 0);

	trace_dptxport_validate_connection(dptx, core, atc, die);
	dptx->validate_calls++;
	dev_dbg(service->ep->dcp->dev,
		 "DPTX validate: call #%u this boot target=0x%x core=%u atc=%u die=%u attrs=0x%x caller=%pS\n",
		 dptx->validate_calls, target, core, atc, die,
		 attrs, __builtin_return_address(0));

	cmd.target = cpu_to_le32(target);
	cmd.unk = cpu_to_le32(attrs);
	ret = afk_service_call(service, 0, 12, &cmd, sizeof(cmd), 40, &resp,
			       sizeof(resp), 40);
	if (ret)
		return ret;

	if (le32_to_cpu(resp.target) != target) {
		dev_warn(service->ep->dcp->dev,
			 "validate_connection: target reply 0x%x (sent 0x%x)\n",
			 le32_to_cpu(resp.target), target);
		return -EINVAL;
	}
	if (le32_to_cpu(resp.unk) != attrs) {
		/* only the Thunderbolt DP IN role is allowed to differ */
		if (!dcp_is_usb4_output(service->ep->dcp)) {
			dev_warn(service->ep->dcp->dev,
				 "validate_connection: attrs reply 0x%x (sent 0x%x), rejecting\n",
				 le32_to_cpu(resp.unk), attrs);
			return -EINVAL;
		}
		dev_info(service->ep->dcp->dev,
			 "validate_connection: attrs reply 0x%x (sent 0x%x)\n",
			 le32_to_cpu(resp.unk), attrs);
	}

	return 0;
}

int dptxport_connect(struct apple_epic_service *service, u8 core, u8 atc,
		     u8 die, bool supports_hpd)
{
	struct dptx_port *dptx = service->cookie;
	struct dcpdptx_connection_cmd cmd, resp;
	/* same role bit as dptxport_validate_connection() above */
	u32 unk_field = (supports_hpd ? DCPDPTX_REMOTE_PORT_SUPPORTS_HPD : 0) |
			(dcp_is_usb4_output(service->ep->dcp) ? 1 : 0);
	int ret;
	u32 target = dptxport_remote_target(service->ep->dcp, core, atc, die);

	trace_dptxport_connect(dptx, core, atc, die);
	dptx->connect_calls++;
	dev_dbg(service->ep->dcp->dev,
		 "DPTX connect: call #%u this boot target=0x%x unk=0x%x caller=%pS\n",
		 dptx->connect_calls, target, unk_field,
		 __builtin_return_address(0));

	cmd.target = cpu_to_le32(target);
	cmd.unk = cpu_to_le32(unk_field);
	ret = afk_service_call(service, 0, 11, &cmd, sizeof(cmd), 24, &resp,
			       sizeof(resp), 24);
	if (ret)
		return ret;

	if (le32_to_cpu(resp.target) != target) {
		dev_warn(service->ep->dcp->dev,
			 "connect: target reply 0x%x (sent 0x%x)\n",
			 le32_to_cpu(resp.target), target);
		return -EINVAL;
	}
	if (le32_to_cpu(resp.unk) != unk_field)
		dev_notice(service->ep->dcp->dev, "unexpected unk field in reply: 0x%x (0x%x)\n",
			  le32_to_cpu(resp.unk), unk_field);

	return 0;
}

int dptxport_request_display(struct apple_epic_service *service)
{
	struct dptx_port *dptx = service->cookie;
	int ret;

	dptx->request_calls++;
	dev_dbg(service->ep->dcp->dev,
		 "DPTX request_display: call #%u this boot caller=%pS\n",
		 dptx->request_calls, __builtin_return_address(0));
	ret = afk_service_call(service, 0, 6, NULL, 0, 16, NULL, 0, 16);
	dev_dbg(service->ep->dcp->dev,
		 "DPTX request_display: call #%u result=%d\n",
		 dptx->request_calls, ret);
	return ret;
}

int dptxport_release_display(struct apple_epic_service *service)
{
	struct dptx_port *dptx = service->cookie;
	/*
	 * The T8140 external DCP's native RemotePortProxy::displayRelease uses
	 * service group 8 (command 7), unlike the older Linux group 0.  Matched
	 * to the 25G83 capture; displayRequest stays group 0.
	 */
	u16 group = service->ep->dcp->hw.h17p_external ? 8 : 0;
	int ret;

	dptx->release_calls++;
	dev_dbg(service->ep->dcp->dev,
		 "DPTX release_display: call #%u this boot caller=%pS\n",
		 dptx->release_calls, __builtin_return_address(0));
	ret = afk_service_call(service, group, 7, NULL, 0, 16, NULL, 0, 16);
	dev_dbg(service->ep->dcp->dev,
		 "DPTX release_display: call #%u result=%d\n",
		 dptx->release_calls, ret);
	return ret;
}

int dptxport_set_hpd_timeout(struct apple_epic_service *service, bool hpd,
			     unsigned int timeout_ms)
{
	struct dcpdptx_hotplug_cmd cmd, resp;
	int ret;

	memset(&cmd, 0, sizeof(cmd));

	if (hpd)
		cmd.unk = cpu_to_le32(1);

	ret = afk_service_call_timeout(service, 8, 8, &cmd, sizeof(cmd), 12,
				       &resp, sizeof(resp), 12, timeout_ms);
	if (ret)
		return ret;
	if (le32_to_cpu(resp.unk) != hpd) {
		dev_warn(service->ep->dcp->dev,
			 "set_hpd: unk reply 0x%x (sent hpd=%d)\n",
			 le32_to_cpu(resp.unk), hpd);
		return -EINVAL;
	}
	return 0;
}

int dptxport_set_hpd(struct apple_epic_service *service, bool hpd)
{
	return dptxport_set_hpd_timeout(service, hpd, MSEC_PER_SEC);
}

/*
 * T8140 external DCP (H17P): the firmware drives AUX and lane training
 * itself and hands the AP per-lane voltage/pre-emphasis entries to program
 * into the ATC PHY.  Decoded with bounds checks; PHY failures are returned to
 * the firmware.
 */
struct dptxport_training_entry {
	__le32 unknown;
	__le32 voltage;
	__le32 preemphasis;
} __packed;

struct dptxport_apcall_training {
	__le32 retcode;
	u8 pad0[12];
	__le32 count;
	u8 pad1[12];
	struct dptxport_training_entry entries[];
} __packed;

static bool dptxport_has_native_training(struct apple_epic_service *service)
{
	return service->ep->dcp->hw.h17p_external;
}

static int dptxport_set_native_training(struct dptx_port *dptx,
				       const void *data, size_t size,
				       void *reply_data, size_t reply_size)
{
	const struct dptxport_apcall_training *request = data;
	struct dptxport_apcall_training *reply = reply_data;
	union phy_configure_opts opts = {};
	u32 count;
	size_t bytes;
	int ret;

	if (size < sizeof(*request) || reply_size < sizeof(*reply))
		return -EINVAL;
	count = le32_to_cpu(request->count);
	if (count > ARRAY_SIZE(dptx->training_settings) || count > dptx->lane_count ||
	    count == 3)
		return -EINVAL;
	bytes = sizeof(*request) + count * sizeof(request->entries[0]);
	if (size < bytes || reply_size < bytes)
		return -EINVAL;
	for (u32 i = 0; i < count; i++) {
		opts.dp.voltage[i] = le32_to_cpu(request->entries[i].voltage);
		opts.dp.pre[i] = le32_to_cpu(request->entries[i].preemphasis);
		if (opts.dp.voltage[i] > 3 || opts.dp.pre[i] > 3)
			return -EINVAL;
	}
	if (count) {
		if (!dptx->atcphy)
			return -ENODEV;
		opts.dp.link_rate = dptx->phy_ops.dp.link_rate;
		opts.dp.lanes = count;
		opts.dp.set_voltages = 1;
		ret = phy_configure(dptx->atcphy, &opts);
		if (ret)
			return ret;
	}
	for (u32 i = 0; i < count; i++) {
		dptx->training_settings[i][0] = le32_to_cpu(request->entries[i].unknown);
		dptx->training_settings[i][1] = opts.dp.voltage[i];
		dptx->training_settings[i][2] = opts.dp.pre[i];
		dptx->phy_ops.dp.voltage[i] = opts.dp.voltage[i];
		dptx->phy_ops.dp.pre[i] = opts.dp.pre[i];
	}
	memmove(reply, request, bytes);
	reply->retcode = cpu_to_le32(0);
	return 0;
}

static int dptxport_get_native_training(struct dptx_port *dptx,
				       const void *data, size_t size,
				       void *reply_data, size_t reply_size)
{
	const struct dptxport_apcall_training *request = data;
	struct dptxport_apcall_training *reply = reply_data;
	u32 count;
	size_t bytes;

	if (size < sizeof(*request) || reply_size < sizeof(*reply))
		return -EINVAL;
	/* Get takes the requested count at body+0 and returns status at body+0x10. */
	count = le32_to_cpu(request->retcode);
	if (count > ARRAY_SIZE(dptx->training_settings))
		return -EINVAL;
	bytes = sizeof(*reply) + count * sizeof(reply->entries[0]);
	if (reply_size < bytes)
		return -EINVAL;
	memmove(reply, request, sizeof(*reply));
	for (u32 i = 0; i < count; i++) {
		reply->entries[i].unknown = cpu_to_le32(dptx->training_settings[i][0]);
		reply->entries[i].voltage = cpu_to_le32(dptx->training_settings[i][1]);
		reply->entries[i].preemphasis = cpu_to_le32(dptx->training_settings[i][2]);
	}
	reply->count = cpu_to_le32(0);
	return 0;
}

static int dptxport_set_native_lanes(struct dptx_port *dptx, u32 lanes)
{
	union phy_configure_opts opts = {};
	int ret;

	if (lanes > 4 || lanes == 3)
		return -EINVAL;
	if (!dptx->atcphy)
		return -ENODEV;
	opts.dp.lanes = lanes;
	opts.dp.set_lanes = 1;
	ret = phy_configure(dptx->atcphy, &opts);
	if (ret)
		return ret;
	dptx->lane_count = lanes;
	dptx->phy_ops.dp.lanes = lanes;
	dptx->phy_ops.dp.set_lanes = 0;
	return 0;
}

static int
dptxport_call_get_max_drive_settings(struct apple_epic_service *service,
				     void *reply_, size_t reply_size)
{
	struct dptxport_apcall_max_drive_settings *reply = reply_;

	if (reply_size < sizeof(*reply))
		return -EINVAL;

	if (dptxport_has_native_training(service)) {
		/* status 0, then (type 0, max voltage 3, max pre-emphasis 3) */
		__le32 max_pre = cpu_to_le32(3);

		memset(reply, 0, sizeof(*reply));
		reply->max_drive_settings[1] = cpu_to_le32(3);
		memcpy(reply->_unk1, &max_pre, sizeof(max_pre));
		return 0;
	}

	reply->retcode = cpu_to_le32(0);
	reply->max_drive_settings[0] = cpu_to_le32(0x3);
	reply->max_drive_settings[1] = cpu_to_le32(0x3);
	if (service->ep->dcp->fw_compat == DCP_FIRMWARE_V_27_0) {
		/* Protocol-27 drive settings: type, voltage, pre-emphasis. */
		reply->max_drive_settings[0] = cpu_to_le32(0);
		memcpy(reply->_unk1, &reply->max_drive_settings[1], sizeof(__le32));
	}

	return 0;
}

static int
dptxport_call_get_drive_settings(struct apple_epic_service *service,
				     const void *request_, size_t request_size,
				     void *reply_, size_t reply_size)
{
	struct dptx_port *dptx = service->cookie;
	const struct dptxport_apcall_drive_settings *request = request_;
	struct dptxport_apcall_drive_settings *reply = reply_;

	if (dptxport_has_native_training(service))
		return dptxport_get_native_training(dptx, request_, request_size,
						   reply_, reply_size);

	if (reply_size < sizeof(*reply) || request_size < sizeof(*request))
		return -EINVAL;

	*reply = *request;

	/* Clear the rest of the buffer */
	memset(reply_ + sizeof(*reply), 0, reply_size - sizeof(*reply));

	/*
	 * retcode appears to be lane count, seeing 2 for USB-C dp alt mode
	 * with lanes splitted for DP/USB3.
	 */
	if (le32_to_cpu(reply->retcode) != dptx->lane_count)
		dev_err(service->ep->dcp->dev,
			"get_drive_settings: unexpected retcode %d\n",
			reply->retcode);

	reply->retcode = cpu_to_le32(dptx->lane_count);
	reply->unk5 = cpu_to_le32(dptx->drive_settings[0]);
	reply->unk6 = cpu_to_le32(0);
	reply->unk7 = cpu_to_le32(dptx->drive_settings[1]);

	return 0;
}

static int
dptxport_call_set_drive_settings(struct apple_epic_service *service,
				     const void *request_, size_t request_size,
				     void *reply_, size_t reply_size)
{
	struct dptx_port *dptx = service->cookie;
	const struct dptxport_apcall_drive_settings *request = request_;
	struct dptxport_apcall_drive_settings *reply = reply_;

	if (dptxport_has_native_training(service))
		return dptxport_set_native_training(dptx, request_, request_size,
						   reply_, reply_size);

	if (reply_size < sizeof(*reply) || request_size < sizeof(*request))
		return -EINVAL;

	*reply = *request;
	reply->retcode = cpu_to_le32(0);

	dev_info(service->ep->dcp->dev, "set_drive_settings: %d:%d:%d:%d:%d:%d:%d\n",
		 request->unk1, request->unk2, request->unk3, request->unk4,
		 request->unk5, request->unk6, request->unk7);

	dptx->drive_settings[0] = le32_to_cpu(reply->unk5);
	dptx->drive_settings[1] = le32_to_cpu(reply->unk7);

	return 0;
}

static int dptxport_call_get_max_link_rate(struct apple_epic_service *service,
					   void *reply_, size_t reply_size)
{
	struct dptxport_apcall_link_rate *reply = reply_;

	if (reply_size < sizeof(*reply))
		return -EINVAL;

	reply->retcode = cpu_to_le32(0);
	reply->link_rate = cpu_to_le32(LINK_RATE_HBR3);

	return 0;
}

static int dptxport_call_get_max_lane_count(struct apple_epic_service *service,
					   void *reply_, size_t reply_size)
{
	struct dptxport_apcall_lane_count *reply = reply_;
	struct dptx_port *dptx = service->cookie;
	struct apple_dcp *dcp = service->ep->dcp;
	union phy_configure_opts phy_ops;
	int ret;

	if (reply_size < sizeof(*reply))
		return -EINVAL;

	if (!dptx->atcphy) {
		/* USB4 DP IN: no ATC DP PHY to validate. */
		dptx->lane_count = 4;
		reply->retcode = cpu_to_le32(0);
		reply->lane_count = cpu_to_le64(4);
		dev_info(dcp->dev, "get_max_lane_count: USB4 DP IN, 4 lanes\n");
		return 0;
	}

	ret = phy_validate(dptx->atcphy, PHY_MODE_DP, 0, &phy_ops);
	if (ret < 0) {
		dev_err(dcp->dev, "phy_validate failed: %d\n", ret);
		reply->retcode = cpu_to_le32(1);
		reply->lane_count = cpu_to_le64(0);
	} else {
		if (phy_ops.dp.lanes < 2) {
			// phy_validate might return 0 lanes if atc phy is not
			// yet switched to DP mode
			dev_dbg(dcp->dev, "get_max_lane_count: phy lanes: %d\n",
				phy_ops.dp.lanes);
			// default to 4 lanes
			dptx->lane_count = 4;
		} else {
			dptx->lane_count = phy_ops.dp.lanes;
		}
		reply->retcode = cpu_to_le32(0);
		reply->lane_count = cpu_to_le64(dptx->lane_count);
	}

	return 0;
}

static int dptxport_call_set_active_lane_count(struct apple_epic_service *service,
					       const void *data, size_t data_size,
					       void *reply_, size_t reply_size)
{
	struct dptx_port *dptx = service->cookie;
	struct apple_dcp *dcp = service->ep->dcp;
	const struct dptxport_apcall_set_active_lane_count *request = data;
	struct dptxport_apcall_set_active_lane_count *reply = reply_;
	int ret = 0;
	int retcode = 0;

	if (reply_size < sizeof(*reply))
		return -1;
	if (data_size < sizeof(*request))
		return -1;

	u64 lane_count = le64_to_cpu(request->lane_count);

	if (dptxport_has_native_training(service)) {
		if (lane_count > 4)
			return -EINVAL;
		ret = dptxport_set_native_lanes(dptx, lane_count);
		if (ret)
			return ret;
		reply->retcode = cpu_to_le32(0);
		reply->lane_count = cpu_to_le64(lane_count);
		if (lane_count)
			complete(&dptx->linkcfg_completion);
		return 0;
	}

	if (dptx->lane_count < lane_count)
		dev_err(dcp->dev, "set_active_lane_count: unexpected lane "
			"count:%llu phy: %d\n", lane_count, dptx->lane_count);

	switch (lane_count) {
	case 0 ... 2:
	case 4:
		dptx->phy_ops.dp.lanes = lane_count;
		/* USB4 DP IN has no ATC PHY; still accept the lane count. */
		dptx->phy_ops.dp.set_lanes =
			dcp_is_usb4_output(dcp) || (dcp->dptx_phy > 3);
		break;
	default:
		dev_err(dcp->dev, "set_active_lane_count: invalid lane count:%llu\n", lane_count);
		retcode = 1;
		lane_count = 0;
		break;
	}

	if (dptx->phy_ops.dp.set_lanes) {
		if (dptx->atcphy && !dcp_is_usb4_output(dcp)) {
			ret = phy_configure(dptx->atcphy, &dptx->phy_ops);
			if (ret)
				return ret;
		}
		dptx->phy_ops.dp.set_lanes = 0;
		dptx->lane_count = lane_count;
	}

	reply->retcode = cpu_to_le32(retcode);
	reply->lane_count = cpu_to_le64(lane_count);

	if (lane_count > 0) {
		dev_info(dcp->dev, "USB4/DPTX: SET_ACTIVE_LANE_COUNT %llu\n",
			 lane_count);
		/*
		 * dcp_usb4_drm_allowed() (usb4_force_dptx) used to gate this
		 * for the old manual-training sysfs knob (module_param_cb
		 * usb4_dptx_train), removed wholesale in 0dc9f50 when
		 * apple_dcp_tb_dp_tunnel() replaced it with automatic tunnel
		 * detection -- but usb4_force_dptx itself, and this gate,
		 * were left behind with no way left to ever set it true.
		 * dcp_dptx_connect()'s single wait_for_completion_timeout()
		 * on linkcfg_completion is now shared by both the alt-mode
		 * and USB4-tunnel paths (also part of 0dc9f50's unification),
		 * so gating it here meant every tunneled connect timed out
		 * after a fully successful DPRX/AUX handshake and lane
		 * negotiation -- firmware does not send FORCE_HOTPLUG_DETECT
		 * (the only other completion site) for a tunnel. The
		 * reference (aurora-silicon/linux#8) completes
		 * linkcfg_completion here unconditionally; usb4_lane_completion
		 * has no consumer anywhere in this tree, so this drops both the
		 * dead gate and the dead completion and matches the reference.
		 */
		complete(&dptx->linkcfg_completion);
	}

	return ret;
}

static int dptxport_call_get_link_rate(struct apple_epic_service *service,
				       void *reply_, size_t reply_size)
{
	struct dptx_port *dptx = service->cookie;
	struct dptxport_apcall_link_rate *reply = reply_;

	if (reply_size < sizeof(*reply))
		return -EINVAL;

	reply->retcode = cpu_to_le32(0);
	reply->link_rate = cpu_to_le32(dptx->link_rate);

	return 0;
}

static int
dptxport_call_will_change_link_config(struct apple_epic_service *service)
{
	struct dptx_port *dptx = service->cookie;

	dptx->phy_ops.dp.set_lanes = 0;
	dptx->phy_ops.dp.set_rate = 0;
	dptx->phy_ops.dp.set_voltages = 0;

	return 0;
}

static int
dptxport_call_did_change_link_config(struct apple_epic_service *service)
{
	/*
	 * Ported from aurora-silicon/linux#8: bringing the tunnel crossbar
	 * connection up (dcp_tunnel_crossbar_up()) now happens in
	 * dptxport_call(), after this succeeds, gated on
	 * dptx_tunnel && link_rate -- not inside this handler, and its
	 * result is never propagated back to DCP as a call failure.
	 */

	/* assume the link config did change and wait a little bit */
	mdelay(10);

	return 0;
}

static int dptxport_call_set_link_rate(struct apple_epic_service *service,
				       const void *data, size_t data_size,
				       void *reply_, size_t reply_size)
{
	struct dptx_port *dptx = service->cookie;
	const struct dptxport_apcall_link_rate *request = data;
	struct dptxport_apcall_link_rate *reply = reply_;
	u32 link_rate, phy_link_rate;
	bool phy_set_rate = false;
	int ret;

	if (reply_size < sizeof(*reply))
		return -EINVAL;
	if (data_size < sizeof(*request))
		return -EINVAL;

	link_rate = le32_to_cpu(request->link_rate);
	trace_dptxport_call_set_link_rate(dptx, link_rate);
	dev_info(service->ep->dcp->dev, "DPTXPort: SET_LINK_RATE 0x%x\n",
		 link_rate);

	switch (link_rate) {
	case LINK_RATE_RBR:
		phy_link_rate = 1620;
		phy_set_rate = true;
		break;
	case LINK_RATE_HBR:
		phy_link_rate = 2700;
		phy_set_rate = true;
		break;
	case LINK_RATE_HBR2:
		phy_link_rate = 5400;
		phy_set_rate = true;
		break;
	case LINK_RATE_HBR3:
		phy_link_rate = 8100;
		phy_set_rate = true;
		break;
	case 0:
		phy_link_rate = 0;
		phy_set_rate = true;
		break;
	default:
		dev_err(service->ep->dcp->dev,
			"DPTXPort: Unsupported link rate 0x%x requested\n",
			link_rate);
		link_rate = 0;
		phy_set_rate = false;
		break;
	}

	if (phy_set_rate) {
		/*
		 * Ported from aurora-silicon/linux#8: a Thunderbolt DP tunnel
		 * starts/stops its own pixel clock instead of configuring the
		 * PHY directly, and never fails the apcall over it -- a
		 * failed clock just keeps the crossbar connection down
		 * (dcp->tb_clock_ok), which dcp_tunnel_crossbar_up() checks.
		 */
		if (dptx->atcphy && service->ep->dcp->dptx_tunnel) {
			dcp_tunnel_set_rate(service->ep->dcp, dptx->atcphy,
					    link_rate);
		} else if (dptx->atcphy) {
			dptx->phy_ops.dp.link_rate = phy_link_rate;
			dptx->phy_ops.dp.set_rate = 1;
			ret = phy_configure(dptx->atcphy, &dptx->phy_ops);
			if (ret)
				return ret;
		}

		dptx->link_rate = dptx->pending_link_rate = link_rate;
	}

	//dptx->pending_link_rate = link_rate;
	reply->retcode = cpu_to_le32(0);
	reply->link_rate = cpu_to_le32(link_rate);

	return 0;
}

static int dptxport_call_get_supports_hpd(struct apple_epic_service *service,
					  void *reply_, size_t reply_size)
{
	struct apple_dcp *dcp = service->ep->dcp;
	struct dptxport_apcall_get_support *reply = reply_;

	if (reply_size < sizeof(*reply))
		return -EINVAL;

	reply->retcode = cpu_to_le32(0);
	/*
	 * Analog DPIN CORE=1 already uses AFK set_hpd (returns 0).
	 * Denying HPD here made request_display ACTIVATE then 22/24
	 * DEVICE_NOT_STARTED (~5.5s). Advertise HPD so firmware uses
	 * that path instead of waiting for a PHY start.
	 */
	reply->supported = cpu_to_le32(dcp_is_typec_output(dcp) ? 1 : 0);
	dev_info(dcp->dev, "DPTXPort: GET_SUPPORTS_HPD %u usb4=%d\n",
		 le32_to_cpu(reply->supported), dcp_is_usb4_output(dcp));
	return 0;
}

static int
dptxport_call_get_supports_downspread(struct apple_epic_service *service,
				      void *reply_, size_t reply_size)
{
	struct dptxport_apcall_get_support *reply = reply_;

	if (reply_size < sizeof(*reply))
		return -EINVAL;

	reply->retcode = cpu_to_le32(0);
	reply->supported = cpu_to_le32(0);
	return 0;
}

static int dptxport_call_set_tiled_display_hint(void *reply_,
						 size_t reply_size)
{
	struct dptxport_apcall_set_tiled *reply = reply_;

	if (reply_size < sizeof(*reply))
		return -EINVAL;

	reply->retcode = cpu_to_le32(1);
	return 0;
}

static int
dptxport_call_activate(struct apple_epic_service *service,
		       const void *data, size_t data_size,
		       void *reply, size_t reply_size)
{
	struct dptx_port *dptx = service->cookie;
	struct apple_dcp *dcp = service->ep->dcp;

	/*
	 * Ported from aurora-silicon/linux#8: no crossbar, and no PHY mode
	 * change for a tunnel (dptx->atcphy must stay in USB4/TBT mode the
	 * whole connection, see dcp_tunnel_set_rate()). The DP IN adapter is
	 * only woken (DPTX_INACTIVE=0) here, via dcp_tunnel_dpin_activate();
	 * waking it earlier hangs the machine. Report PHY activation failures
	 * to DCP instead of acknowledging a transmitter that is not ready.
	 */
	if (dptx->atcphy && !dcp->phy_managed_by_typec) {
		int ret = phy_set_mode_ext(dptx->atcphy, PHY_MODE_DP, dcp->index);

		if (ret)
			return ret;
	}
	if (dcp->dptx_tunnel)
		dcp_tunnel_dpin_activate(dcp, true);

	memcpy(reply, data, min(reply_size, data_size));
	if (reply_size >= 4)
		memset(reply, 0, 4);

	return 0;
}

static int
dptxport_call_deactivate(struct apple_epic_service *service,
		       const void *data, size_t data_size,
		       void *reply, size_t reply_size)
{
	struct dptx_port *dptx = service->cookie;
	struct apple_dcp *dcp = service->ep->dcp;
	int ret;

	dev_info(dcp->dev, "DPTXPort: DEACTIVATE\n");
	ret = dcp_direct_crossbar_link(dcp, false);
	if (ret)
		return ret;
	if (dcp->dptx_tunnel)
		dcp_tunnel_dpin_activate(dcp, false);
	if (dptx->atcphy && !dcp->phy_managed_by_typec) {
		ret = phy_set_mode_ext(dptx->atcphy, PHY_MODE_INVALID, 0);
		if (ret)
			return ret;
	}

	memcpy(reply, data, min(reply_size, data_size));
	if (reply_size >= 4)
		memset(reply, 0, 4);

	return 0;
}

/*
 * T8140 external DCP: ATC PHY mode for DCP input selection.  Type-C owns the
 * PHY mode when it manages the PHY.  The firmware is acknowledged either way.
 */
static int
dptxport_call_activate_h17p_ext(struct apple_epic_service *service,
				const void *data, size_t data_size,
				void *reply, size_t reply_size)
{
	struct dptx_port *dptx = service->cookie;
	const struct apple_dcp *dcp = service->ep->dcp;

	if (!dcp->phy_managed_by_typec)
		phy_set_mode_ext(dptx->atcphy, PHY_MODE_DP, dcp->index);

	memcpy(reply, data, min(reply_size, data_size));
	if (reply_size >= 4)
		memset(reply, 0, 4);

	return 0;
}

static int
dptxport_call_deactivate_h17p_ext(struct apple_epic_service *service,
				  const void *data, size_t data_size,
				  void *reply, size_t reply_size)
{
	struct dptx_port *dptx = service->cookie;
	const struct apple_dcp *dcp = service->ep->dcp;

	if (!dcp->phy_managed_by_typec)
		phy_set_mode_ext(dptx->atcphy, PHY_MODE_INVALID, 0);

	memcpy(reply, data, min(reply_size, data_size));
	if (reply_size >= 4)
		memset(reply, 0, 4);

	return 0;
}

/*
 * The T8140 external DCP's AP-call table: the classic calls plus native
 * training, without the Thunderbolt tunnel and crossbar link handling of the
 * other DPTX ports.
 */
static int dptxport_call_h17p_ext(struct apple_epic_service *service, u32 idx,
				  const void *data, size_t data_size,
				  void *reply, size_t reply_size)
{
	struct dptx_port *dptx = service->cookie;

	switch (idx) {
	case DPTX_APCALL_WILL_CHANGE_LINKG_CONFIG:
		return dptxport_call_will_change_link_config(service);
	case DPTX_APCALL_DID_CHANGE_LINK_CONFIG:
		return dptxport_call_did_change_link_config(service);
	case DPTX_APCALL_GET_MAX_LINK_RATE:
		return dptxport_call_get_max_link_rate(service, reply,
						       reply_size);
	case DPTX_APCALL_GET_LINK_RATE:
		return dptxport_call_get_link_rate(service, reply, reply_size);
	case DPTX_APCALL_SET_LINK_RATE:
		return dptxport_call_set_link_rate(service, data, data_size,
						   reply, reply_size);
	case DPTX_APCALL_GET_MAX_LANE_COUNT:
		return dptxport_call_get_max_lane_count(service, reply, reply_size);
	case DPTX_APCALL_SET_ACTIVE_LANE_COUNT:
		return dptxport_call_set_active_lane_count(service, data, data_size,
							   reply, reply_size);
	case DPTX_APCALL_GET_SUPPORTS_HPD:
		return dptxport_call_get_supports_hpd(service, reply,
						      reply_size);
	case DPTX_APCALL_GET_SUPPORTS_DOWN_SPREAD:
		return dptxport_call_get_supports_downspread(service, reply,
							     reply_size);
	case DPTX_APCALL_GET_MAX_DRIVE_SETTINGS:
		return dptxport_call_get_max_drive_settings(service, reply,
							    reply_size);
	case DPTX_APCALL_SET_TILED_DISPLAY_HINTS:
		memcpy(reply, data, min(reply_size, data_size));
		return dptxport_call_set_tiled_display_hint(reply, reply_size);
	case DPTX_APCALL_GET_DRIVE_SETTINGS:
		return dptxport_call_get_drive_settings(service, data, data_size,
							reply, reply_size);
	case DPTX_APCALL_SET_DRIVE_SETTINGS:
		return dptxport_call_set_drive_settings(service, data, data_size,
							reply, reply_size);
	case DPTX_APCALL_ACTIVATE:
		return dptxport_call_activate_h17p_ext(service, data, data_size,
						       reply, reply_size);
	case DPTX_APCALL_DEACTIVATE:
		return dptxport_call_deactivate_h17p_ext(service, data, data_size,
							 reply, reply_size);
	case DPTX_APCALL_DEVICE_NOT_RESPONDING:
	case DPTX_APCALL_DEVICE_BUSY_TIMEOUT:
	case DPTX_APCALL_DEVICE_NOT_STARTED:
		/* The firmware telling us the link failed. */
		dev_warn(service->ep->dcp->dev,
			 "DPTXPort: firmware reports link fault %u on %u:%u\n",
			 idx, service->ep->dcp->dptx_die,
			 service->ep->dcp->dptx_phy);
		memcpy(reply, data, min(reply_size, data_size));
		if (reply_size >= 4)
			memset(reply, 0, 4);
		return 0;
	case DPTX_APCALL_SINK_QUERY_25G83:
		/* Accepted with success and a zeroed retcode, as natively. */
		memcpy(reply, data, min(reply_size, data_size));
		if (reply_size >= 4)
			memset(reply, 0, 4);
		return 0;
	default:
		/*
		 * Not implemented here.  The reply echoes the request with a
		 * zero retcode, which is what the native client does for the
		 * calls it accepts without a payload.  Reported once per index
		 * per port: the firmware repeats these throughout link training.
		 */
		if (idx < 32 && !(dptx->unhandled_logged & BIT(idx))) {
			dptx->unhandled_logged |= BIT(idx);
			dev_warn(service->ep->dcp->dev,
				 "DPTXPort: AP call %u is not implemented; echoing the request with a zero retcode\n",
				 idx);
		}
		fallthrough;
	case DPTX_APCALL_GET_DOWN_SPREAD:
	case DPTX_APCALL_SET_DOWN_SPREAD:
		memcpy(reply, data, min(reply_size, data_size));
		if (reply_size >= 4)
			memset(reply, 0, 4);
		return 0;
	}
}

static int dptxport_call(struct apple_epic_service *service, u32 idx,
			 const void *data, size_t data_size, void *reply,
			 size_t reply_size)
{
	struct dptx_port *dptx = service->cookie;
	trace_dptxport_apcall(dptx, idx, data_size);
	dev_dbg(service->ep->dcp->dev, "DPTXPort: APCALL %u (%zu bytes)\n",
		 idx, data_size);
	if (data_size)
		print_hex_dump_debug("DPTXPort: apcall data: ",
				     DUMP_PREFIX_OFFSET, 16, 1, data,
				     min(data_size, (size_t)64), true);

	if (service->ep->dcp->hw.h17p_external)
		return dptxport_call_h17p_ext(service, idx, data, data_size,
					      reply, reply_size);

	switch (idx) {
	case DPTX_APCALL_WILL_CHANGE_LINKG_CONFIG: {
		int ret = dcp_direct_crossbar_link(service->ep->dcp, false);

		if (ret)
			return ret;
		/*
		 * Ported from aurora-silicon/linux#8: a re-link on an
		 * established tunnel takes the crossbar connection down
		 * first.
		 */
		if (service->ep->dcp->dptx_tunnel && dptx->link_rate)
			dcp_tunnel_crossbar_down(service->ep->dcp);
		return dptxport_call_will_change_link_config(service);
	}
	case DPTX_APCALL_DID_CHANGE_LINK_CONFIG: {
		int ret = dptxport_call_did_change_link_config(service);

		if (!ret && service->ep->dcp->dptx_tunnel && dptx->link_rate)
			dcp_tunnel_crossbar_up(service->ep->dcp);
		if (!ret && dptx->link_rate)
			ret = dcp_direct_crossbar_link(service->ep->dcp, true);
		return ret;
	}
	case DPTX_APCALL_GET_MAX_LINK_RATE:
		return dptxport_call_get_max_link_rate(service, reply,
						       reply_size);
	case DPTX_APCALL_GET_LINK_RATE:
		return dptxport_call_get_link_rate(service, reply, reply_size);
	case DPTX_APCALL_SET_LINK_RATE:
		return dptxport_call_set_link_rate(service, data, data_size,
						   reply, reply_size);
	case DPTX_APCALL_GET_MAX_LANE_COUNT:
		return dptxport_call_get_max_lane_count(service, reply, reply_size);
	case DPTX_APCALL_GET_ACTIVE_LANE_COUNT: {
		struct dptxport_apcall_lane_count *lc = reply;

		if (reply_size < sizeof(*lc))
			return -EINVAL;
		lc->retcode = cpu_to_le32(0);
		lc->lane_count = cpu_to_le64(dptx->lane_count);
		return 0;
	}
        case DPTX_APCALL_SET_ACTIVE_LANE_COUNT:
		return dptxport_call_set_active_lane_count(service, data, data_size,
							   reply, reply_size);
	case DPTX_APCALL_GET_SUPPORTS_HPD:
		return dptxport_call_get_supports_hpd(service, reply,
						      reply_size);
	case DPTX_APCALL_GET_SUPPORTS_DOWN_SPREAD:
		return dptxport_call_get_supports_downspread(service, reply,
							     reply_size);
	case DPTX_APCALL_GET_MAX_DRIVE_SETTINGS:
		return dptxport_call_get_max_drive_settings(service, reply,
							    reply_size);
	case DPTX_APCALL_DEVICE_NOT_RESPONDING:
	case DPTX_APCALL_DEVICE_BUSY_TIMEOUT:
	case DPTX_APCALL_DEVICE_NOT_STARTED:
		dev_warn(service->ep->dcp->dev,
			 "DPTXPort: firmware reports link fault %u on target %u:%u\n",
			 idx, service->ep->dcp->dptx_die,
			 service->ep->dcp->dptx_phy);
		memcpy(reply, data, min(reply_size, data_size));
		return 0;
	case DPTX_APCALL_SET_TILED_DISPLAY_HINTS:
		if (service->ep->dcp->fw_compat == DCP_FIRMWARE_V_27_0)
			return dcp_peer_hints_v27(service->ep->dcp, dptx, data,
						  data_size, reply, reply_size);
		memcpy(reply, data, min(reply_size, data_size));
		return dptxport_call_set_tiled_display_hint(reply, reply_size);
	case DPTX_APCALL_GET_DRIVE_SETTINGS:
		if (service->ep->dcp->fw_compat == DCP_FIRMWARE_V_27_0)
			return dptxport_call_drive_v27(service, false, data, data_size,
						     reply, reply_size);
		return dptxport_call_get_drive_settings(service, data, data_size,
							reply, reply_size);
	case DPTX_APCALL_SET_DRIVE_SETTINGS:
		if (service->ep->dcp->fw_compat == DCP_FIRMWARE_V_27_0)
			return dptxport_call_drive_v27(service, true, data, data_size,
						     reply, reply_size);
		return dptxport_call_set_drive_settings(service, data, data_size,
							reply, reply_size);
        case DPTX_APCALL_ACTIVATE:
		return dptxport_call_activate(service, data, data_size,
					      reply, reply_size);
	case DPTX_APCALL_DEACTIVATE:
		return dptxport_call_deactivate(service, data, data_size,
						reply, reply_size);
	case DPTX_APCALL_FORCE_HOTPLUG_DETECT:
		dev_info(service->ep->dcp->dev,
			 "DPTXPort: FORCE_HOTPLUG_DETECT\n");
		complete(&dptx->linkcfg_completion);
		memcpy(reply, data, min(reply_size, data_size));
		if (reply_size >= 4)
			memset(reply, 0, 4);
		return 0;
	case DPTX_APCALL_INACTIVE_SINK_DETECTED:
		/*
		 * Normal prelude to link training on USB4. Ack and wait for
		 * SET_ACTIVE_LANE_COUNT. Treating this as failure made
		 * firmware DEACTIVATE after it had already set lanes/rate.
		 */
		dev_info(service->ep->dcp->dev,
			 "DPTXPort: INACTIVE_SINK_DETECTED (keep waiting for lanes)\n");
		memcpy(reply, data, min(reply_size, data_size));
		if (reply_size >= 4)
			memset(reply, 0, 4);
		return 0;
	default:
		/* just try to ACK and hope for the best... */
		dev_info(service->ep->dcp->dev, "DPTXPort: acking unhandled call %u\n",
			idx);
		fallthrough;
	case DPTX_APCALL_GET_DOWN_SPREAD:
	case DPTX_APCALL_SET_DOWN_SPREAD:
		memcpy(reply, data, min(reply_size, data_size));
		if (reply_size >= 4)
			memset(reply, 0, 4);
		return 0;
	}
}

static void dptxport_init_props(struct apple_epic_service *service, s64 unit,
			       const void *props, size_t size)
{
	struct apple_dcp *dcp = service->ep->dcp;
	struct dcp_parse_ctx ctx;
	struct dcp_dptx_caps caps;

	/* The T8140 external DCP's units are interface ids, not port indices. */
	if (dcp->hw.h17p_external)
		return;

	if (unit < 0 || unit >= ARRAY_SIZE(dcp->dptxport) ||
	    parse(props, size, &ctx) || parse_dptx_caps(&ctx, &caps))
		return;
	dcp->dptxport[unit].caps = caps;
	dev_info(dcp->dev, "DPTX%lld caps: width=%u total=%u active=%u\n",
		 unit, caps.width, caps.total_rate, caps.active_rate);
}

static void dptxport_init_h17p_ext(struct apple_epic_service *service,
				   const char *name, const char *class, s64 unit)
{
	struct apple_dcp *dcp = service->ep->dcp;
	s64 port;

	if (strcmp(name, "dcpdptx-port-epic")) {
		dev_warn(dcp->dev,
			 "DPTXPort: ignoring service named '%s', expected 'dcpdptx-port-epic'\n",
			 name);
		return;
	}
	if (strcmp(class, "AppleDCPDPTXRemotePort")) {
		dev_warn(dcp->dev,
			 "DPTXPort: service '%s' has class '%s', expected 'AppleDCPDPTXRemotePort'\n",
			 name, class);
		return;
	}

	trace_dptxport_init(dcp, unit);

	/*
	 * On H17P the announced "unit" is the dynamic EPIC interface id, not a
	 * zero-based port index.  The connect path only ever uses
	 * dptxport[0..num_dptx_ports); on this single-port controller map the
	 * sole RemotePort to index 0 so a nonzero interface id does not make
	 * it miss dptxport[0].
	 */
	if (dcp->fw_compat == DCP_FIRMWARE_H17P && dcp->hw.num_dptx_ports == 1)
		port = 0;
	else
		port = unit;

	switch (port) {
	case 0:
	case 1:
		if (dcp->dptxport[port].enabled) {
			dev_err(dcp->dev,
				"DPTXPort: unit %lld (port %lld) already exists\n",
				unit, port);
			return;
		}
		dcp->dptxport[port].unit = port;
		dcp->dptxport[port].service = service;
		dcp->dptxport[port].enabled = true;
		service->cookie = (void *)&dcp->dptxport[port];
		complete(&dcp->dptxport[port].enable_completion);
		dev_info(dcp->dev,
			 "DPTXPort: enabled port %lld from announced unit %lld\n",
			 port, unit);
		break;
	default:
		dev_err(dcp->dev, "DPTXPort: invalid unit %lld\n", unit);
	}
}

static void dptxport_init(struct apple_epic_service *service, const char *name,
			  const char *class, s64 unit)
{
	if (service->ep->dcp->hw.h17p_external)
		return dptxport_init_h17p_ext(service, name, class, unit);

	if (strcmp(name, "dcpdptx-port-epic"))
		return;
	if (strcmp(class, "AppleDCPDPTXRemotePort"))
		return;

	trace_dptxport_init(service->ep->dcp, unit);

	switch (unit) {
	case 0:
	case 1:
		if (service->ep->dcp->dptxport[unit].enabled) {
			dev_err(service->ep->dcp->dev,
				"DPTXPort: unit %lld already exists\n", unit);
			return;
		}
		service->ep->dcp->dptxport[unit].unit = unit;
		service->ep->dcp->dptxport[unit].service = service;
		service->ep->dcp->dptxport[unit].enabled = true;
		service->cookie = (void *)&service->ep->dcp->dptxport[unit];
		complete(&service->ep->dcp->dptxport[unit].enable_completion);
		break;
	default:
		dev_err(service->ep->dcp->dev, "DPTXPort: invalid unit %lld\n",
			unit);
	}
}

/*
 * The DisplayPort controller services of the T8140 external DCP.  They are
 * not driven beyond power and hotplug -- starting their endpoint at all is the
 * point, because the firmware will not bring up the DP stack (and so never
 * announces "dcpdptx-port-epic") until they are up.  Matching is by
 * EPICProviderClass.
 */
static void dp_control_init(struct apple_epic_service *service,
			    const char *name, const char *class, s64 unit)
{
	struct apple_dcp *dcp = service->ep->dcp;

	dev_info(dcp->dev,
		 "J700_DP_CONTROL: ep:%02x service '%s' class '%s' unit %lld\n",
		 service->ep->endpoint, name, class, unit);

	/*
	 * Keep them apart by EPIC name: both announce as AppleDCPDPTXController,
	 * on units 1 ("dcpav-controller-epic") and 3 ("dcpdp-controller-epic").
	 * Do not call into them from here -- this runs on the AFK work queue
	 * that would have to deliver the reply.
	 */
	if (!strcmp(name, "dcpav-controller-epic"))
		dcp->dptxctl_av = service;
	else if (!strcmp(name, "dcpdp-controller-epic"))
		dcp->dptxctl_dp = service;
}

/*
 * Command numbers on the controller service (DCPAVControllerService): group
 * 8, with setPower=0x8, getPower=0x9, wakeDisplay=0xa, sleepDisplay=0xb,
 * forceHotPlugDetect=0xc, setVirtualDeviceMode=0xd.  The payload shape is the
 * same <16xI12x> that dcpdptx_hotplug_cmd already describes.
 */
#define DPTXCTL_SET_POWER		0x8
#define DPTXCTL_WAKE_DISPLAY		0xa
#define DPTXCTL_FORCE_HPD		0xc

static int dptxctl_call(struct apple_dcp *dcp, struct apple_epic_service *svc,
			const char *what, u32 command, u32 arg)
{
	struct dcpdptx_hotplug_cmd cmd, resp;
	int ret;

	memset(&cmd, 0, sizeof(cmd));
	memset(&resp, 0, sizeof(resp));
	cmd.unk = cpu_to_le32(arg);

	ret = afk_service_call(svc, 8, command, &cmd, sizeof(cmd), 12, &resp,
			       sizeof(resp), 12);
	dev_info(dcp->dev, "J700_DPTXCTL: %s(cmd=%#x arg=%u) -> %d resp=%#x\n",
		 what, command, arg, ret, le32_to_cpu(resp.unk));
	return ret;
}

/*
 * Nudge the DisplayPort controller after IOMFB is up.  Binding the controller
 * services is not on its own enough to make the firmware publish
 * "dcpdptx-port-epic": it powers the external pipe straight back down ("HPD
 * not present, skipping mode export").  Ask it to power up and re-run hotplug
 * detection.
 */
void dptx_control_kick(struct apple_dcp *dcp)
{
	if (dcp->dptxctl_av) {
		dptxctl_call(dcp, dcp->dptxctl_av, "av.setPower",
			     DPTXCTL_SET_POWER, 1);
		dptxctl_call(dcp, dcp->dptxctl_av, "av.wakeDisplay",
			     DPTXCTL_WAKE_DISPLAY, 0);
		dptxctl_call(dcp, dcp->dptxctl_av, "av.forceHotPlugDetect",
			     DPTXCTL_FORCE_HPD, 0);
	} else {
		dev_info(dcp->dev, "J700_DPTXCTL: no dcpav controller bound\n");
	}

	if (dcp->dptxctl_dp)
		dptxctl_call(dcp, dcp->dptxctl_dp, "dp.setPower",
			     DPTXCTL_SET_POWER, 1);
	else
		dev_info(dcp->dev, "J700_DPTXCTL: no dcpdp controller bound\n");
}

/*
 * Matching is by EPICProviderClass, not by the EPIC service name.  Endpoint
 * 0x24 announces two services, both of class "AppleDCPDPTXController", on
 * channels 1 and 3.
 */
static const struct apple_epic_service_ops dptx_control_ops[] = {
	{ .name = "AppleDCPDPTXController", .init = dp_control_init },
	{}
};

/*
 * Start the DisplayPort controller endpoint.  Errors are not fatal: the
 * caller carries on to dptxep_init() either way.
 */
int dptx_control_init(struct apple_dcp *dcp)
{
	int ret;

	dcp->dptxcontrolep = afk_init(dcp, DPTX_CONTROL_ENDPOINT,
				      dptx_control_ops);
	if (IS_ERR(dcp->dptxcontrolep))
		return PTR_ERR(dcp->dptxcontrolep);

	ret = afk_start(dcp->dptxcontrolep);
	if (ret) {
		dev_warn(dcp->dev, "Failed to start DPTX control endpoint: %d\n",
			 ret);
		return ret;
	}

	return 0;
}

static const struct apple_epic_service_ops dptxep_ops[] = {
	{
		.name = "AppleDCPDPTXRemotePort",
		.init = dptxport_init,
		.init_props = dptxport_init_props,
		.call = dptxport_call,
	},
	{}
};

int dptxep_init(struct apple_dcp *dcp)
{
	int ret;
	u32 port;
	unsigned long timeout = msecs_to_jiffies(1000);
	u32 endpoint;

	init_completion(&dcp->dptxport[0].enable_completion);
	init_completion(&dcp->dptxport[1].enable_completion);
	init_completion(&dcp->dptxport[0].linkcfg_completion);
	init_completion(&dcp->dptxport[1].linkcfg_completion);

	if (dcp->hw.h17p_external)
		endpoint = DPTX_PORT_ENDPOINT_H17P;
	else if (dcp->fw_compat == DCP_FIRMWARE_V_27_0)
		endpoint = DPTX_ENDPOINT_V27;
	else
		endpoint = DPTX_ENDPOINT;

	dcp->dptxep = afk_init(dcp, endpoint, dptxep_ops);
	if (IS_ERR(dcp->dptxep))
		return PTR_ERR(dcp->dptxep);

	ret = afk_start(dcp->dptxep);
	if (ret)
		return ret;

	/*
	 * The T8140 external DCP does not announce its port service until
	 * IOMFB is running; dcp_start() waits for it with dptxep_wait_ports()
	 * after iomfb_start_rtkit().
	 */
	if (dcp->hw.h17p_external)
		return 0;

	for (port = 0; port < dcp->hw.num_dptx_ports; port++) {
		ret = wait_for_completion_timeout(&dcp->dptxport[port].enable_completion,
						timeout);
		if (!ret)
			return -ETIMEDOUT;
		else if (ret < 0)
			return ret;
		timeout = ret;
	}

	return 0;
}

/*
 * Wait for the T8140 external DCP to publish its DisplayPort port service.
 *
 * The firmware does not announce "dcpdptx-port-epic" until IOMFB is running,
 * and IOMFB cannot start while a wait inside dptxep_init() holds the caller:
 * measured on J700 the two events are 3 ms apart in exactly the wrong order.
 * Starting the endpoint is still done early, so a service announcement always
 * has somewhere to land; only the wait moves after IOMFB.
 */
int dptxep_wait_ports(struct apple_dcp *dcp)
{
	unsigned long timeout = msecs_to_jiffies(1000);
	u32 port;
	int ret;

	if (IS_ERR_OR_NULL(dcp->dptxep))
		return -ENODEV;

	for (port = 0; port < dcp->hw.num_dptx_ports; port++) {
		if (dcp->dptxport[port].enabled)
			continue;
		ret = wait_for_completion_timeout(&dcp->dptxport[port].enable_completion,
						timeout);
		if (!ret)
			return -ETIMEDOUT;
		else if (ret < 0)
			return ret;
		timeout = ret;
	}

	return 0;
}
