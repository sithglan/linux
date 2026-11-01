#!/usr/bin/env python3
"""Ownership proof obligations, NOT a model verified against Apple firmware.

Assumption under test: installation may retain a root independently of the
current command. Successful work proves that work finished; absent a detach
contract, it does not prove the installed reference was replaced. This model
also exposes the cumulative capacity limit of stop-only graph reclamation.
It deliberately does not claim that the observed hardware uses this cache.
"""
from dataclasses import dataclass
from pathlib import Path
import re
import unittest


limits = (Path(__file__).resolve().parents[1] / "g17_queue_limits.rs").read_text()


def constant(name):
    return int(re.search(rf"const {name}: u32 = (\d+);", limits)[1])


CAPACITY = constant("TTBAT_CONTEXT_COUNT") - constant("COMPUTE_CONTEXT_FIRST") - 1


@dataclass
class Installed:
    vm: int
    active: bool = False
    claimed: bool = True


class Session:
    def __init__(self):
        self.graphs = []
        self.stops = 0

    def acquire(self, vm):
        for graph in self.graphs:
            if graph.vm == vm and not graph.claimed and not graph.active:
                graph.claimed = True
                return graph
        if len(self.graphs) == CAPACITY:
            raise BufferError("installed roots remain pinned, including idle owners")
        graph = Installed(vm)
        self.graphs.append(graph)
        return graph

    def close(self, graph):
        graph.claimed = False

    def stop(self, primary_stopped, secondary_stopped):
        if not primary_stopped or not secondary_stopped:
            raise RuntimeError("no complete processor-stop witness")
        self.graphs.clear()
        self.stops += 1


class OwnershipContract(unittest.TestCase):
    def test_same_vm_reuses_immutable_graph_without_reinstall(self):
        session = Session()
        first = session.acquire(1)
        for _ in range(10000):
            first.active = True
            first.active = False
            session.close(first)
            self.assertIs(session.acquire(1), first)
        self.assertEqual(len(session.graphs), 1)
        self.assertEqual(session.stops, 0)

    def test_different_vm_never_retargets_installed_graph(self):
        session = Session()
        old = session.acquire(1)
        session.close(old)
        new = session.acquire(2)
        self.assertIsNot(old, new)
        self.assertEqual(old.vm, 1)

    def test_full_simultaneous_capacity_is_preserved(self):
        session = Session()
        for vm in range(CAPACITY):
            session.acquire(vm).active = True
        self.assertEqual(sum(g.active for g in session.graphs), CAPACITY)
        self.assertEqual(session.stops, 0)

    def test_stop_only_reclamation_exhausts_on_serial_client_churn(self):
        session = Session()
        for vm in range(CAPACITY):
            graph = session.acquire(vm)
            graph.active = True
            graph.active = False
            session.close(graph)
        self.assertFalse(any(g.active or g.claimed for g in session.graphs))
        with self.assertRaises(BufferError):
            session.acquire(CAPACITY)
        self.assertEqual(CAPACITY, 58)

    def test_only_complete_processor_stop_reclaims_installed_ownership(self):
        session = Session()
        old = session.acquire(1)
        session.close(old)
        for primary, secondary in [(False, False), (True, False), (False, True)]:
            with self.assertRaises(RuntimeError):
                session.stop(primary, secondary)
            self.assertEqual(session.graphs, [old])
        session.stop(True, True)
        self.assertEqual(session.graphs, [])
        self.assertEqual(session.acquire(2).vm, 2)

    def test_work_completion_does_not_imply_cached_root_detachment(self):
        # Counterexample to that inference, not a claim about actual firmware.
        installed_root = 1
        command_root = 2
        owned_roots = {installed_root, command_root}
        completed_command_root = command_root
        self.assertEqual(completed_command_root, 2)
        owned_roots.remove(1)  # previous transactional completion rule
        self.assertNotIn(installed_root, owned_roots)


if __name__ == "__main__":
    unittest.main()
