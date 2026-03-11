"""Hardware canary test harness — lightweight helpers for running braid on real disks."""

import contextlib
import os
import shlex
import subprocess
import sys

CONFIG = "/tmp/braid-hw-test/config.json"
MOUNT_POINT = "/mnt/braid-hw-test"


# ---------------------------------------------------------------------------
# Shell helpers
# ---------------------------------------------------------------------------

def run(cmd, timeout=300):
    """Run a command, assert exit 0, return stdout."""
    result = subprocess.run(
        cmd, shell=True, capture_output=True, text=True, timeout=timeout,
    )
    if result.returncode != 0:
        combined = (result.stdout + result.stderr).strip()
        raise AssertionError(
            f"Command failed (exit {result.returncode}): {cmd}\n{combined}"
        )
    return result.stdout


def run_fail(cmd, timeout=300):
    """Run a command, assert non-zero exit, return combined output."""
    result = subprocess.run(
        cmd, shell=True, capture_output=True, text=True, timeout=timeout,
    )
    combined = (result.stdout + result.stderr).strip()
    if result.returncode == 0:
        raise AssertionError(
            f"Expected failure but got exit 0: {cmd}\n{combined}"
        )
    return combined


def run_capture(cmd, timeout=300):
    """Run a command, return (exit_code, combined_output)."""
    result = subprocess.run(
        cmd, shell=True, capture_output=True, text=True, timeout=timeout,
    )
    combined = (result.stdout + result.stderr).strip()
    return result.returncode, combined


# ---------------------------------------------------------------------------
# Disk accessors
# ---------------------------------------------------------------------------

def disk(n):
    """Return the nth device path from BRAID_HW_DISKS (1-indexed)."""
    disks = os.environ["BRAID_HW_DISKS"].split(":")
    return disks[n - 1]


def disk_name(n):
    """Return the braid key for the nth disk."""
    return f"hwtest{n}"


# ---------------------------------------------------------------------------
# Cleanup
# ---------------------------------------------------------------------------

def cleanup():
    """Best-effort umount + close mappers for hw-test disk names only."""
    subprocess.run(f"umount {MOUNT_POINT} 2>/dev/null", shell=True)
    # Only close mappers for hwtest names we use in tests
    for i in range(1, 10):
        name = f"braid-hwtest{i}"
        subprocess.run(
            f"cryptsetup close {name} 2>/dev/null", shell=True,
        )


# ---------------------------------------------------------------------------
# Command builders
# ---------------------------------------------------------------------------

PASSPHRASE = "testpassphrase"
LUKS_OPTS = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add_cmd(key):
    """Build a `braid add <key> --yes` command with env vars and --config."""
    pq = shlex.quote(PASSPHRASE)
    return (
        f"printf '%s\\n' {pq} | "
        f"BRAID_LUKS_OPTS='{LUKS_OPTS}' "
        f"braid add {key} --passphrase-stdin --yes --config {CONFIG}"
    )


def replace_cmd(old, new, extra=""):
    """Build a `braid replace` command."""
    pq = shlex.quote(PASSPHRASE)
    return (
        f"printf '%s\\n' {pq} | "
        f"BRAID_LUKS_OPTS='{LUKS_OPTS}' "
        f"braid replace --old {old} --new {new} "
        f"--passphrase-stdin --yes --config {CONFIG} {extra}"
    )


def unlock_cmd(extra=""):
    """Build a `braid unlock` command."""
    pq = shlex.quote(PASSPHRASE)
    return (
        f"printf '%s\\n' {pq} | "
        f"braid unlock --passphrase-stdin --config {CONFIG} {extra}"
    )


def remove_cmd(key, extra=""):
    """Build a `braid remove` command."""
    return f"braid remove {key} --yes --config {CONFIG} {extra}"


def lock_cmd():
    """Build a `braid lock` command."""
    return f"braid lock --config {CONFIG}"


# ---------------------------------------------------------------------------
# Test section context manager
# ---------------------------------------------------------------------------

@contextlib.contextmanager
def section(name):
    """Context manager that prints test section name and PASS/FAIL."""
    print(f"\n{'='*60}")
    print(f"  {name}")
    print(f"{'='*60}")
    sys.stdout.flush()
    try:
        yield
        print(f"  PASS: {name}")
    except Exception:
        print(f"  FAIL: {name}")
        raise
    finally:
        sys.stdout.flush()
