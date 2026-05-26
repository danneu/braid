# Test: braid remove-missing -- ENOSPC pre-flight fails closed
#
# Intent: verify that `braid remove-missing` refuses when the ENOSPC
# relocation-space pre-flight cannot validate `btrfs device usage --raw`,
# in both dry-run and real-run.
#
# Why it exists: this command runs on a degraded pool. If the pre-flight
# cannot prove survivor relocation capacity, proceeding can hit ENOSPC
# mid-relocation, force the filesystem read-only, and strand
# pending-op.json. The refusal fires inside `plan_remove_missing`,
# before `journal::write_journal`; the absence of pending-op.json after
# the real-run refusal is the proof of fail-closed behavior.
#
# Scenario: 3-disk RAID1 pool, disk3 dies, pool mounted degraded. A PATH
# wrapper intercepts `btrfs device usage --raw` and fails the single
# call issued by `check_relocation_space`. Dry-run and real-run both
# exit nonzero, leave the missing device in btrfs, and the real-run
# leaves no journal behind.

import base64
import json
import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add_cmd(key):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 {key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes"
    )


def get_missing_devid():
    raw = machine.succeed("braid status --json")
    report = json.loads(raw)
    devids = report.get("missing_devids", [])
    assert len(devids) > 0, "No missing devids in braid status:\n" + raw
    return str(devids[0])


# --- Phase 1: Build 3-drive RAID1 pool ---

with subtest("Setup: build 3-drive pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed(add_cmd("disk3"))
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in ["braid-disk1", "braid-disk2", "braid-disk3"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing:\n{fi_show}"

# --- Phase 2: Simulate disk3 death, mount degraded ---

with subtest("Simulate disk3 death and mount degraded"):
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk3")
    machine.succeed("mount -o degraded /dev/mapper/braid-disk1 /mnt/storage")
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "missing" in fi_show.lower(), f"expected missing device:\n{fi_show}"

missing_devid = get_missing_devid()

# --- Phase 3: Install a wrapper that fails `btrfs device usage --raw`
#     inside check_relocation_space.
#
# `braid` in nixpkgs is wrapped by makeWrapper with `--prefix PATH :
# ${toolPath}`, which forcibly prepends btrfs-progs to PATH. A plain
# `PATH=/tmp/wrap:$PATH braid ...` invocation therefore never reaches
# our shim -- toolPath's btrfs wins the lookup. To bypass that, we
# resolve the underlying (unwrapped) braid binary from the wrapper
# script and exec it directly with PATH=<our-wrap>:$PATH, so our
# shim-btrfs comes first.

import re

braid_wrapped_path = machine.succeed("readlink -f $(command -v braid)").strip()
wrapper_source = machine.succeed(f"cat {braid_wrapped_path}")
m = re.search(r'(/nix/store/[^"\s]+/bin/braid)(?!\-)', wrapper_source)
assert m, f"could not locate unwrapped braid in wrapper:\n{wrapper_source}"
unwrapped_braid = m.group(1)
print(f"unwrapped braid: {unwrapped_braid}")

real_btrfs = machine.succeed("command -v btrfs").strip()

wrapper_template = """#!/usr/bin/env bash
set -eu
COUNTER="/tmp/btrfs-wrap-count"
if [ "${1:-}" = "device" ] && [ "${2:-}" = "usage" ] && [ "${3:-}" = "--raw" ]; then
    n=$(cat "$COUNTER" 2>/dev/null || echo 0)
    n=$((n + 1))
    echo "$n" > "$COUNTER"
    echo "simulated btrfs failure (call $n)" >&2
    exit 1
fi
exec __REAL_BTRFS__ "$@"
"""
wrapper_script = wrapper_template.replace("__REAL_BTRFS__", real_btrfs)
wrapper_b64 = base64.b64encode(wrapper_script.encode()).decode()

machine.succeed(
    "mkdir -p /tmp/wrap && "
    f"printf '%s' {shlex.quote(wrapper_b64)} | base64 -d > /tmp/wrap/btrfs && "
    "chmod +x /tmp/wrap/btrfs"
)


def run_with_wrapper(args):
    """Reset the call counter and invoke unwrapped braid with /tmp/wrap
    first on PATH so our shim-btrfs wins the lookup.

    `args` is the braid command arguments (everything after `braid`),
    including any shell redirections.
    """
    machine.succeed("rm -f /tmp/btrfs-wrap-count")
    shell_cmd = f"export PATH=/tmp/wrap:$PATH; {unwrapped_braid} {args}"
    return machine.execute(shell_cmd)


# --- Phase 4: Dry-run must refuse and leave the pool unchanged ---

with subtest("dry-run: preflight failure refuses before plan render"):
    (status, _) = run_with_wrapper(
        f"remove-missing --missing-id {missing_devid} --dry-run "
        ">/tmp/out 2>/tmp/err"
    )
    assert status != 0, f"dry-run should fail; exit {status}"
    out = machine.succeed("cat /tmp/out")
    err = machine.succeed("cat /tmp/err")
    assert (
        "btrfs device usage failed (exit 1): simulated btrfs failure" in err
    ), f"expected validation message on stderr; got:\n{err}"
    assert "ENOSPC pre-flight" not in err, (
        f"btrfs command failure should surface btrfs stderr directly; got:\n{err}"
    )
    assert "[long" not in out, f"dry-run must not render mutation steps:\n{out}"
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "missing" in fi_show.lower(), (
        f"dry-run refusal must leave missing device in pool:\n{fi_show}"
    )

# --- Phase 5: Real-run must refuse before writing a journal ---

with subtest("real-run: preflight failure refuses before journal"):
    (status, _) = run_with_wrapper(
        f"remove-missing --missing-id {missing_devid} --yes "
        ">/tmp/out2 2>/tmp/err2"
    )
    assert status != 0, f"real-run should fail; exit {status}"
    err2 = machine.succeed("cat /tmp/err2")
    assert (
        "btrfs device usage failed (exit 1): simulated btrfs failure" in err2
    ), f"expected validation message on stderr; got:\n{err2}"
    assert "[wait] pool: removing missing devid" not in err2, (
        f"real-run must fail before the mutating btrfs remove starts:\n{err2}"
    )
    assert "\x1b[" not in err2, f"real-run stderr must be plain without a TTY; got:\n{err2}"
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "missing" in fi_show.lower(), (
        f"real-run refusal must leave missing device in pool:\n{fi_show}"
    )
    machine.fail("test -f /var/lib/braid/pending-op.json")

machine.shutdown()
