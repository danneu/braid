#!/usr/bin/env python3
"""Hardware canary test orchestrator.

Usage:
    # Read disks from existing braid config (interactive confirm):
    sudo python3 tests/hw/runner.py --from-config /etc/braid/config.json

    # Explicit device paths (interactive confirm):
    sudo python3 tests/hw/runner.py \\
        /dev/disk/by-id/usb-ABC /dev/disk/by-id/usb-DEF /dev/disk/by-id/usb-GHI

    # Skip confirmation (CI / scripted):
    sudo python3 tests/hw/runner.py --from-config /etc/braid/config.json \\
        --yes-destroy-these-disks

    # Run specific tests:
    sudo python3 tests/hw/runner.py --from-config /etc/braid/config.json \\
        --tests test_add_canary test_lock_unlock_canary
"""

import argparse
import json
import os
import stat
import subprocess
import sys
import time

TESTS_DIR = os.path.dirname(os.path.abspath(__file__))
CONFIG_DIR = "/tmp/braid-hw-test"
CONFIG_PATH = os.path.join(CONFIG_DIR, "config.json")
RUNTIME_STATE = "/var/lib/braid"

ALL_TESTS = [
    "test_add_canary",
    "test_lock_unlock_canary",
    "test_replace_live_canary",
    "test_remove_under_pressure",
]


def die(msg):
    print(f"ERROR: {msg}", file=sys.stderr)
    sys.exit(1)


def disk_info(path):
    """Return (model, size) for a block device."""
    real = os.path.realpath(path)
    dev_name = os.path.basename(real)

    model = "unknown"
    size = "unknown"
    try:
        model_path = f"/sys/block/{dev_name}/device/model"
        if os.path.exists(model_path):
            with open(model_path) as f:
                model = f.read().strip()
    except OSError:
        pass
    try:
        size_path = f"/sys/block/{dev_name}/size"
        if os.path.exists(size_path):
            with open(size_path) as f:
                sectors = int(f.read().strip())
                size_gb = (sectors * 512) / (1024**3)
                size = f"{size_gb:.1f} GB"
    except (OSError, ValueError):
        pass

    return model, size


def devices_from_config(config_path):
    """Read disk by_id paths from a braid config file, sorted by key name."""
    try:
        with open(config_path) as f:
            config = json.load(f)
    except (OSError, json.JSONDecodeError) as e:
        die(f"Cannot read config {config_path}: {e}")

    disks = config.get("disks", {})
    if not disks:
        die(f"No disks found in {config_path}")

    # Sort by key name for deterministic ordering
    paths = []
    for key in sorted(disks.keys()):
        by_id = disks[key].get("by_id")
        if not by_id:
            die(f"Disk '{key}' in {config_path} has no by_id field")
        paths.append(by_id)

    return paths


def validate_devices(paths):
    """Validate that all paths are block devices."""
    for p in paths:
        if not os.path.exists(p):
            die(f"Device does not exist: {p}")
        real = os.path.realpath(p)
        if not stat.S_ISBLK(os.stat(real).st_mode):
            die(f"Not a block device: {p} -> {real}")


def print_devices(paths):
    """Print device table with model/size info."""
    print("Devices:")
    for i, p in enumerate(paths, 1):
        model, size = disk_info(p)
        print(f"  disk{i}: {p}")
        print(f"          {model}, {size}")
    print()


def confirm_destroy(paths):
    """Print warning and prompt for confirmation. Returns True if confirmed."""
    print("WARNING: This will DESTROY ALL DATA on these disks:")
    print()
    for p in paths:
        model, size = disk_info(p)
        print(f"  {p}  ({model}, {size})")
    print()
    print("Each test wipes all disks. Your braid pool will be gone.")
    print()

    try:
        answer = input("Type 'destroy' to confirm: ")
    except (EOFError, KeyboardInterrupt):
        print()
        return False

    return answer.strip() == "destroy"


def write_config(device_paths):
    """Write test config to /tmp/braid-hw-test/config.json."""
    os.makedirs(CONFIG_DIR, exist_ok=True)
    config = {
        "disks": {},
        "mount_point": "/mnt/storage",
    }
    for i, path in enumerate(device_paths, 1):
        config["disks"][f"disk{i}"] = {"by_id": path}

    with open(CONFIG_PATH, "w") as f:
        json.dump(config, f, indent=2)


def cleanup():
    """Best-effort umount + close mappers for test disk names."""
    subprocess.run("umount /mnt/storage 2>/dev/null", shell=True)
    for i in range(1, 10):
        subprocess.run(f"cryptsetup close braid-disk{i} 2>/dev/null", shell=True)


def wipe_disks(device_paths):
    """Wipe filesystem signatures and first 10 MiB of each disk."""
    for p in device_paths:
        subprocess.run(f"wipefs -a {p} 2>/dev/null", shell=True)
        subprocess.run(
            f"dd if=/dev/zero of={p} bs=1M count=10 2>/dev/null", shell=True,
        )


def reset_state():
    """Remove braid runtime state."""
    subprocess.run(f"rm -rf {RUNTIME_STATE}", shell=True)


def run_test(test_name, device_paths):
    """Run a single test as a subprocess. Returns True on success."""
    test_file = os.path.join(TESTS_DIR, f"{test_name}.py")
    if not os.path.exists(test_file):
        print(f"  Test file not found: {test_file}")
        return False

    env = os.environ.copy()
    env["BRAID_HW_DISKS"] = ":".join(device_paths)

    result = subprocess.run(
        [sys.executable, test_file],
        env=env,
    )
    return result.returncode == 0


def main():
    parser = argparse.ArgumentParser(
        description="Run hardware canary tests (DESTRUCTIVE to specified disks)",
    )
    parser.add_argument(
        "devices", nargs="*", metavar="DEVICE",
        help="Block device paths (need exactly 3)",
    )
    parser.add_argument(
        "--from-config", metavar="PATH",
        help="Read disk paths from a braid config file",
    )
    parser.add_argument(
        "--tests", nargs="+", metavar="TEST",
        help="Run only specific tests",
    )
    parser.add_argument(
        "--yes-destroy-these-disks", action="store_true",
        help="Skip interactive confirmation",
    )
    args = parser.parse_args()

    # Resolve device paths
    if args.from_config and args.devices:
        die("Use --from-config or positional devices, not both")
    elif args.from_config:
        device_paths = devices_from_config(args.from_config)
    elif args.devices:
        device_paths = args.devices
    else:
        die("Provide device paths or --from-config")

    if len(device_paths) != 3:
        die(f"Expected exactly 3 devices, got {len(device_paths)}")

    if os.geteuid() != 0:
        die("Must run as root")

    validate_devices(device_paths)
    print_devices(device_paths)

    # Confirm destruction
    if not args.yes_destroy_these_disks:
        if not confirm_destroy(device_paths):
            print("Aborted.")
            sys.exit(1)
        print()

    tests_to_run = args.tests if args.tests else ALL_TESTS
    for t in tests_to_run:
        if t not in ALL_TESTS:
            die(f"Unknown test: {t}\nAvailable: {', '.join(ALL_TESTS)}")

    print(f"Running {len(tests_to_run)} test(s): {', '.join(tests_to_run)}")
    print()

    results = {}
    start_time = time.time()

    for test_name in tests_to_run:
        print(f"\n{'#'*60}")
        print(f"# {test_name}")
        print(f"{'#'*60}\n")

        # Pre-test reset
        cleanup()
        wipe_disks(device_paths)
        reset_state()
        write_config(device_paths)

        test_start = time.time()
        passed = run_test(test_name, device_paths)
        elapsed = time.time() - test_start

        results[test_name] = passed
        status = "PASS" if passed else "FAIL"
        print(f"\n  {status}: {test_name} ({elapsed:.1f}s)")

    # Summary
    total_time = time.time() - start_time
    print(f"\n{'='*60}")
    print(f"  SUMMARY ({total_time:.1f}s)")
    print(f"{'='*60}")
    passed_count = sum(1 for v in results.values() if v)
    failed_count = len(results) - passed_count
    for name, passed in results.items():
        status = "PASS" if passed else "FAIL"
        print(f"  {status}: {name}")
    print(f"\n  {passed_count} passed, {failed_count} failed")

    sys.exit(0 if failed_count == 0 else 1)


if __name__ == "__main__":
    main()
