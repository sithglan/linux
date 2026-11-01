// SPDX-License-Identifier: GPL-2.0-only OR MIT
/* Copyright 2021 Alyssa Rosenzweig */

#ifndef __APPLE_DCP_H__
#define __APPLE_DCP_H__

#include <drm/drm_atomic.h>
#include <drm/drm_encoder.h>
#include <drm/drm_fourcc.h>

#include "connector.h"
#include "dcp-internal.h"
#include "parser.h"

struct apple_crtc {
	struct drm_crtc base;
	struct drm_pending_vblank_event *event;
	bool vsync_disabled;

	/* Reference to the DCP device owning this CRTC */
	struct platform_device *dcp;
};

#define to_apple_crtc(x) container_of(x, struct apple_crtc, base)

struct apple_encoder {
	struct drm_encoder base;
};

#define to_apple_encoder(x) container_of(x, struct apple_encoder, base)

void dcp_poweroff(struct platform_device *pdev);
void dcp_poweron(struct platform_device *pdev);
int dcp_peer_hints_v27(struct apple_dcp *dcp, struct dptx_port *port,
		      const void *data, size_t size, void *reply, size_t reply_size);
bool dcp_peer_tiling_state_v27(struct apple_dcp *dcp, bool write,
			      u32 event, u32 param, u32 *value);
void dcp_peer_display_ids_v27(struct apple_dcp *dcp, u32 own, u32 related);
int dcp_peer_modeset_v27(struct apple_dcp *dcp, u32 timing, u32 color, bool vrr);
int dcp_peer_color_v27(struct apple_dcp *dcp, struct iomfb_set_matrix_req *matrix);
int dcp_peer_stop_v27(struct apple_dcp *dcp);
struct apple_dcp *dcp_peer_scanout_v27(struct apple_dcp *dcp);
void dcp_peer_swap_armed_v27(struct apple_dcp *dcp, u32 id);
void dcp_peer_swap_failed_v27(struct apple_dcp *dcp);
bool dcp_peer_swap_complete_v27(struct apple_dcp *dcp, u32 id, ktime_t now,
		void (*report)(struct apple_dcp *, u32, ktime_t));
int dcp_set_crc(struct drm_crtc *crtc, bool enabled);
int dcp_crtc_atomic_check(struct drm_crtc *crtc, struct drm_atomic_commit *state);
int dcp_get_connector_type(struct platform_device *pdev);
bool dcp_has_typec_routes(struct platform_device *pdev);

/*
 * The Type-C display fabric.  Ports are enumerated in device-tree order, not
 * DCP probe order, so a given physical port keeps the same DRM connector index
 * across boots -- userspace keys its per-monitor configuration on that name.
 */
unsigned int dcp_typec_nr_ports(void);
struct device_node *dcp_typec_port_of_node(unsigned int idx);
bool dcp_typec_port_has_candidate(unsigned int idx, struct platform_device *pdev);
void dcp_typec_port_set_connector(unsigned int idx, bool secondary,
				  struct apple_connector *connector);
bool dcp_typec_dual_stream(void);
void dcp_typec_reorder(void);
bool dcp_is_typec_only(struct platform_device *pdev);
bool dcp_fw_compat_is_12_x(struct platform_device *pdev);
unsigned long* dcp_get_iomfb_surfaces(struct platform_device *pdev);
const u32 *dcp_get_iomfb_surface_order(struct platform_device *pdev,
				       u32 *count);
void dcp_link(struct platform_device *pdev, struct apple_crtc *apple,
	      struct apple_connector *connector);
int dcp_start(struct platform_device *pdev);
int dcp_wait_ready(struct platform_device *pdev, u64 timeout);
void dcp_flush(struct drm_crtc *crtc, struct drm_atomic_commit *state);
void dcp_retain_framebuffer(struct platform_device *pdev,
			    struct dcp_fb_reference *entry);
void dcp_arm_retained_framebuffers(struct apple_dcp *dcp, u32 swap_id);
void dcp_unarm_retained_framebuffers(struct apple_dcp *dcp, u32 swap_id);
void dcp_release_retained_framebuffers(struct apple_dcp *dcp, u32 swap_id);
void dcp_release_all_retained_framebuffers(struct apple_dcp *dcp);
bool dcp_is_initialized(struct platform_device *pdev);
void apple_crtc_vblank(struct apple_crtc *apple);
void dcp_drm_crtc_vblank(struct apple_crtc *crtc);
int dcp_get_modes(struct drm_connector *connector);
enum drm_mode_status dcp_mode_valid(struct drm_connector *connector,
				    const struct drm_display_mode *mode);
int dcp_crtc_atomic_modeset(struct drm_crtc *crtc,
			    struct drm_atomic_commit *state);
bool dcp_crtc_mode_fixup(struct drm_crtc *crtc,
			 const struct drm_display_mode *mode,
			 struct drm_display_mode *adjusted_mode);
void dcp_set_dimensions(struct apple_dcp *dcp);
void dcp_send_message(struct apple_dcp *dcp, u8 endpoint, u64 message);

int dcp_dptx_connect_oob(struct platform_device *pdev, u32 port);
int dcp_dptx_disconnect_oob(struct platform_device *pdev, u32 port);

struct apple_dcp;
struct phy;
/* Thunderbolt DP tunnels, called from the DPTX endpoint */
int dcp_tunnel_crossbar_up(struct apple_dcp *dcp);
int dcp_tunnel_crossbar_down(struct apple_dcp *dcp);
int dcp_tunnel_set_rate(struct apple_dcp *dcp, struct phy *phy, u32 link_rate);
int dcp_tunnel_dpin_activate(struct apple_dcp *dcp, bool active);

int iomfb_start_rtkit(struct apple_dcp *dcp);
void iomfb_shutdown(struct apple_dcp *dcp);
/* rtkit message handler for IOMFB messages */
void iomfb_recv_msg(struct apple_dcp *dcp, u64 message);

int systemep_init(struct apple_dcp *dcp);
int dptxep_init(struct apple_dcp *dcp);
int dptxep_wait_ports(struct apple_dcp *dcp);
int dptx_control_init(struct apple_dcp *dcp);
void dptx_control_kick(struct apple_dcp *dcp);
int ibootep_init(struct apple_dcp *dcp);
int dpavservep_init(struct apple_dcp *dcp);
int avep_init(struct apple_dcp *dcp);


void __init dcp_register(void);
void __exit dcp_unregister(void);

void __init dcp_audio_register(void);
void __exit dcp_audio_unregister(void);

#endif
