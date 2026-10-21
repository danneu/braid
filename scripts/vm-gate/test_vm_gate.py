"""Tests for the host-wide VM gate (plan/2026-10-21-1047/PO5).

Lightweight shell commands stand in for VM tests.

Run: python3 -m unittest discover -s scripts/vm-gate
"""

from __future__ import annotations

import os
import re
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
GATE = HERE / "vm_gate.py"
JUSTFILE = HERE.parent.parent / "justfile"


class GateTest(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.dir = Path(tmp.name)
        self.log = self.dir / "log"
        self.log.touch()
        self.env = {**os.environ, "BRAID_VM_GATE": str(self.dir / "gate.lock")}

    def gated(self, script: str) -> subprocess.Popen:
        return subprocess.Popen([sys.executable, str(GATE), "sh", "-c", script], env=self.env,
                                stderr=subprocess.PIPE, text=True)

    def events(self) -> list[str]:
        return self.log.read_text().split()

    def wait_for(self, event: str) -> None:
        deadline = time.monotonic() + 10
        while event not in self.events():
            self.assertLess(time.monotonic(), deadline, f"{event} never logged")
            time.sleep(0.02)

    def step(self, name: str) -> str:
        return f"echo start-{name} >> {self.log}; sleep 0.5; echo end-{name} >> {self.log}"

    def test_separate_invocations_take_turns(self):
        first = self.gated(self.step("a"))
        self.wait_for("start-a")
        second = self.gated(self.step("b"))
        self.assertEqual(first.wait(), 0)
        self.assertEqual(second.wait(), 0)
        self.assertEqual(self.events(), ["start-a", "end-a", "start-b", "end-b"])
        self.assertIn("waiting", second.stderr.read())
        first.stderr.close()
        second.stderr.close()

    def test_concurrency_inside_one_invocation_is_unchanged(self):
        proc = self.gated(f"({self.step('x')}) & ({self.step('y')}) & wait")
        self.assertEqual(proc.wait(), 0)
        proc.stderr.close()
        self.assertEqual(sorted(self.events()[:2]), ["start-x", "start-y"])

    def test_a_killed_holder_releases_the_gate(self):
        crashed = self.gated("kill -9 $$")
        crashed.wait()
        crashed.stderr.close()
        after = self.gated(f"echo ran >> {self.log}")
        self.assertEqual(after.wait(), 0)
        self.assertNotIn("waiting", after.stderr.read())
        after.stderr.close()
        self.assertEqual(self.events(), ["ran"])

    def test_exit_status_passes_through(self):
        proc = self.gated("exit 3")
        self.assertEqual(proc.wait(), 3)
        proc.stderr.close()


class JustfileTest(unittest.TestCase):
    # Commands that build or run something other than a VM: release
    # packages and nixpkgs tools.
    NOT_A_VM = re.compile(r"\.#packages\.|nixpkgs#")

    def test_every_vm_booting_command_takes_the_gate(self):
        ungated = []
        for n, line in enumerate(JUSTFILE.read_text().splitlines(), 1):
            code = line.strip()
            if code.startswith("#") or not re.search(r"\bnix (build|run)\b", code):
                continue
            if code.startswith("{{vm_gate}} ") or self.NOT_A_VM.search(code):
                continue
            ungated.append(f"justfile:{n}: {code}")
        self.assertEqual(ungated, [], "VM-booting commands must start with {{vm_gate}}")


if __name__ == "__main__":
    unittest.main()
