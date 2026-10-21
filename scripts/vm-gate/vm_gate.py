#!/usr/bin/env python3
"""Run a command while holding the host-wide VM gate (plan/2026-10-21-1047/D10).

VM tests boot QEMU on the Mac itself, so RAM is the shared resource. One
recipe already runs several VMs at once under its own `--max-jobs` cap;
this gate makes separate invocations (concurrent implement runs, or the
user) take turns. Concurrency inside the wrapped command is untouched.

Usage: python3 scripts/vm-gate/vm_gate.py <command> [args...]

The lock is an flock(2) on a file under $TMPDIR (override with
BRAID_VM_GATE). The command is exec'd with the locked descriptor
inherited, so the kernel releases the gate when the command and every
child holding the descriptor exit -- a crash can never leave it stuck.
"""

from __future__ import annotations

import fcntl
import os
import sys
import tempfile
from pathlib import Path


def lock_path() -> Path:
    override = os.environ.get("BRAID_VM_GATE")
    if override:
        return Path(override)
    return Path(tempfile.gettempdir()) / "braid-vm-gate.lock"


def main(argv: list[str]) -> int:
    if not argv:
        print("usage: vm_gate.py <command> [args...]", file=sys.stderr)
        return 2
    path = lock_path()
    fd = os.open(path, os.O_RDWR | os.O_CREAT, 0o644)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        holder = os.pread(fd, 512, 0).decode(errors="replace").strip() or "unknown"
        print(f"vm-gate: waiting for another VM run to finish (holder: {holder}; lock: {path})",
              file=sys.stderr, flush=True)
        fcntl.flock(fd, fcntl.LOCK_EX)
    os.ftruncate(fd, 0)
    os.pwrite(fd, f"pid {os.getpid()} in {os.getcwd()}: {' '.join(argv)}\n".encode(), 0)
    os.set_inheritable(fd, True)
    try:
        os.execvp(argv[0], argv)
    except OSError as e:
        print(f"vm-gate: cannot run {argv[0]}: {e}", file=sys.stderr)
        return 127


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
