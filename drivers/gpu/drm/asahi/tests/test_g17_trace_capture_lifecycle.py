#!/usr/bin/env python3
"""Source boundary regressions; pure archive bytes are tested by Rust itself."""
from pathlib import Path
import unittest

SOURCE = Path(__file__).resolve().parents[1]


def function(text, name):
    start = text.index("fn " + name + "(")
    body = text.index("{", start)
    depth = 1
    end = body + 1
    while depth:
        depth += (text[end] == "{") - (text[end] == "}")
        end += 1
    return text[body:end]


class CaptureLifecycle(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.live = (SOURCE / "g17_live_boot.rs").read_text()
        cls.manager = (SOURCE / "g17_manager.rs").read_text()
        cls.drm = (SOURCE / "g17_drm.rs").read_text()
        cls.parameters = (SOURCE / "asahi.rs").read_text()

    def test_binary_capture_is_explicit_and_default_off(self):
        parameter = self.parameters[self.parameters.index("g17p_binary_capture: u32"):]
        self.assertRegex(parameter[:300], r"default:\s*0,")
        constructor = function(self.manager, "new_with_uat")
        self.assertLess(
            constructor.index("g17p_binary_capture.value()"),
            constructor.index("G17PTraceDump::new"),
        )

        ktrace = self.parameters[self.parameters.index("g17p_fw_ktrace_verbose: u32"):]
        self.assertRegex(ktrace[:300], r"default:\s*0,")

    def test_prepared_scan_precedes_flist_wake(self):
        body = function(self.live, "submit_translated_render_inner")  # v92d body
        begin = body.index(".begin_translated_render(")
        scan = body.index(".capture_prepared_render_trace(")
        flist = body.index("self.publish_render_usc_freelist_request(render_slot)")
        self.assertLess(begin, scan)
        self.assertLess(scan, flist)
        self.assertEqual(body.count(".capture_prepared_render_trace("), 1)
        self.assertIn("native_doorbell_mode == 0 && !split_launch", body[scan:flist])

    def test_firmware_code_capture_is_opt_in_read_only_and_boot_only(self):
        dump = self.manager[self.manager.index("impl G17PTraceDump {"):]
        body = function(dump, "new")
        self.assertLess(body.index("g17p_binary_capture.value() & 2"),
                        body.index("context0_upper_root()"))
        self.assertIn("copy_firmware_range(0, output.len())", body)
        self.assertIn("meta.expected_len = 0x4_c000", body)
        for forbidden in ("set_bits(", "write_u64(", "g17p_gfx_link_to_physical"):
            self.assertNotIn(forbidden, body)

    def test_unprepared_boot_archive_follows_first_real_slot(self):
        # The boot archive stays with the session until a render prepares
        # it. Preparing attaches it to the selected slot, and every later
        # observation or export reads only that slot's archive, so it
        # travels with that graph and no other slot ever sees it.
        body = function(self.manager, "capture_prepared_render_trace")
        attach = body.index("if self.render_slots.get(pair_slot).trace_capture.is_none()")
        decision = body[attach:attach + 340]
        self.assertIn("self.trace_capture.take()", decision)
        self.assertIn("self.render_slots.get_mut(pair_slot).trace_capture = Some(capture)", decision)
        self.assertLess(attach, body.index("if capture.prepared { return Ok(()); }"))
        self.assertLess(body.index("if capture.prepared { return Ok(()); }"),
                        body.index("capture.prepared = true"))
        for name in ("capture_render_trace_observation", "finish_render_trace"):
            self.assertNotIn("self.trace_capture", function(self.manager, name))
        select = self.manager[self.manager.index("impl G17PRenderSlots {"):]
        select = select[:select.index("\n}\n")]
        self.assertNotIn("trace_capture", select)

    def test_halt_scan_after_validation_before_ack(self):
        body = function(self.live, "complete_primary_real_recovery")
        scan = body.index("finish_g17p_render_trace(")
        self.assertLess(body.index("manager.validate_empty_primary_firmware_recovery(generation)?"), scan)
        self.assertLess(body.index("if cause.host_recovery != 0"), scan)
        self.assertLess(scan, body.index("manager.acknowledge_primary_firmware_recovery(1, 2)?"))

    def test_compute_fault_tail_is_drained_before_ack(self):
        body = function(self.live, "complete_primary_real_recovery")
        drain = body.index('probe_g17p_ktrace(pdev.as_ref(), manager, "fault-before-ack")')
        self.assertLess(body.index("manager.validate_empty_primary_firmware_recovery(generation)?"), drain)
        self.assertLess(drain, body.index("manager.acknowledge_primary_firmware_recovery(1, 2)?"))
        probe = function(self.live, "probe_g17p_ktrace")
        self.assertLess(probe.index("if !g17p_ktrace_enabled()"), probe.index("manager.drain_ktrace_ring"))

    def test_irq_completion_keeps_ktrace_consumer_moving(self):
        body = function(self.live, "service_primary_firmware_events")
        drain = body.index("manager.drain_ktrace_ring(role, verbose, budget)")
        sample = body.index("manager.read_compute_completion()")
        self.assertLess(drain, sample)
        # v89: the consumer runs by default (native drainFirmwareRings); the
        # capture mask only gates printing. Trace precedes statistics.
        self.assertLess(body.index("if native_rings || g17p_ktrace_enabled()"), drain)
        statistics = body.index("manager.drain_statistics_ring_native(role)")
        self.assertLess(drain, statistics)
        self.assertLess(statistics, sample)
        legacy_pending = body.index("if self.pending_compute.is_some()")
        self.assertLess(legacy_pending, sample)
        self.assertNotIn("read_compute_completion", body[drain:legacy_pending])

    def test_both_completion_paths_capture_before_flist_release(self):
        body = function(self.live, "wait_render_completion_servicing_events")
        self.assertEqual(body.count("finish_g17p_render_trace("), 2)
        chunks = body.split("finish_g17p_render_trace(")[1:]
        for chunk in chunks:
            self.assertIn("Phase::AfterPairRetirement", chunk)
            self.assertLess(chunk.index("Phase::AfterPairRetirement"),
                            chunk.index(".retire_render_usc_freelist_at_completion(render_slot)?"))

    def test_terminal_export_takes_owner_and_seals_before_return(self):
        body = function(self.manager, "finish_render_trace")
        self.assertIn("trace::Phase::AfterPairRetirement | trace::Phase::RecoveryBeforeAck", body)
        self.assertIn("capture.prepared", body)
        self.assertLess(body.index("self.capture_render_trace_observation"), body.index("self.render_slots.get_mut(pair_slot).trace_capture.take()"))
        self.assertLess(body.index("archive.seal()"), body.index("Ok(Some(capture))"))
        self.assertNotIn("GFP_", body)

    def test_no_new_mmio_or_physical_alias_in_capture_methods(self):
        for name in ("capture_prepared_render_trace", "capture_render_trace_observation", "finish_render_trace"):
            body = function(self.manager, name)
            for forbidden in ("Mmio", "ioremap", "LiveFirmware", "map_at(", "pr_info!", "dev_info!"):
                self.assertNotIn(forbidden, body)

    def test_render_capture_uses_execution_context_and_retained_aliases(self):
        source = (SOURCE / "g17_resources.rs").read_text()
        source = source[source.index("impl G17PUserRenderStorage {"):]
        body = function(source, "capture_owned_trace")
        self.assertIn("let context = u32::from(self.descriptor_context_id)", body)
        self.assertNotIn("self._command_context.context_id()", body)
        for owner in ("deflake", "aux_fb"):
            self.assertIn("&self.render_user_aliases." + owner, body)
        for owner in ("rt_memory", "tpc"):
            self.assertIn("self.render_target_memory." + owner + ".capture_trace_range(", body)
        self.assertIn(".record_offset(self.descriptor_ordinal)", body)
        self.assertIn("self.render_target_memory.tpc_va()", body)
        self.assertIn("self.fragment_status_low.iova()", body)
        self.assertIn("G17P_RENDER_STATE_TA_STATUS", body)
        self.assertIn("self.render_state.capture_trace_range(", body)
        self.assertIn("map(|pm| &mut pm.backing), 0)", body)

    def test_render_submission_uses_the_logical_queue_execution_context(self):
        # Render publication runs in the queue's scheduler backend after the
        # ioctl has installed dependencies and returned.
        body = function(self.drm, "publish_with")  # v84: trait publish delegates here
        call = body[body.index(".submit_translated_render("):]
        self.assertRegex(call[:800], r"\.context\s*\.clone\(\)")
        self.assertNotIn("RENDER_APP_CONTEXT", call[:800])

        source = (SOURCE / "g17_resources.rs").read_text()
        owner = source[source.index("impl G17ResourceOwner {"):]
        constructor = function(source[source.index("impl G17PUninstalledRenderGraph {"):], "build")
        self.assertIn("context.matches(vm)", constructor)
        self.assertIn("context.context_id()", constructor)
        restage = function(source, "restage_command")
        self.assertEqual(
            restage.count("context_id: u32::from(descriptor_context_id)"), 2
        )
        self.assertIn("self.descriptor_context_id = descriptor_context_id", restage)

    def test_boot_capture_is_before_initdata_not_before_rtkit_boot(self):
        start = self.live.index("g17_manager::G17PManagerConstruction::new_with_uat(")
        self.assertLess(start, self.live.index("let handoff = manager.handoff();", start))
        self.assertLess(self.live.index("let handoff = manager.handoff();", start),
                        self.live.index(".send_message_logged(g17_rtkit::ENDPOINT_GFX_MESSAGES, gfx_init)", start))
        self.assertIn("trace::Phase::BootConstructed", function(self.manager[self.manager.index("impl G17PTraceDump"):], "new"))

    def test_pm_low_aliases_are_explicit_context_zero_owners(self):
        source = (SOURCE / "g17_resources.rs").read_text()
        body = function(source[source.index("impl G17ResourceOwner {"):], "capture_owned_trace")
        for owner in ("pb_descriptor_table_low", "uma_page_pool_descriptor_table_low", "partial_primary_index_low",
                      "completion_ordinal_0_low", "completion_ordinal_2_low"):
            self.assertIn("&self.resources." + owner, body)
        self.assertIn("metrics.context = 0", body)
        self.assertIn("metrics.dva = self.resources.parameter_metrics_low_va", body)


if __name__ == "__main__":
    unittest.main()
