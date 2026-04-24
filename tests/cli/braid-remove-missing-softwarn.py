# Test: braid remove-missing -- ENOSPC pre-flight soft-warn stream routing
#
# Intent: pin the stdout/stderr contract for the soft-warn branch of
# `check_relocation_space`. Under `--dry-run` the warning must appear on
# stdout as a `[warn]  <body>` note and stderr must be empty; under
# real-run the same warning must appear on stderr using the canonical
# `[warn]  <body>` wording (no legacy `warning: ` prefix -- plan-derived
# Warn notes now render through the shared
# `preview::render_notes_for_stderr` helper in both modes).
#
# Why it exists: a regression that either (a) leaks the warn on stderr
# during `--dry-run`, (b) drops the `[warn]  ` prefix during real-run,
# or (c) reintroduces the legacy `warning: ` prefix would only surface
# through a human noticing drift in an SSH session. Unit tests catch
# the plan-level shape; this test catches the wire-level stream routing.
#
# Scenario: 3-disk RAID1 pool, disk3 dies, pool mounted degraded. A PATH
# wrapper intercepts `btrfs device usage --raw` and fails the second
# call, which is the one issued by `check_relocation_space` -- the
# first call comes from `probe_missing_devids` and is passed through so
# the --missing-id validation still succeeds.

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
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add {key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes"
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

# --- Phase 3: Install a wrapper that fails the second `btrfs device
#     usage --raw` invocation (the one inside check_relocation_space).
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
    if [ "$n" -ge 2 ]; then
        echo "simulated btrfs failure (call $n)" >&2
        exit 1
    fi
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


# --- Phase 4: Dry-run must route the warn to stdout, leave stderr empty ---

with subtest("dry-run: warn goes to stdout, stderr is empty"):
    (status, _) = run_with_wrapper(
        f"remove-missing --missing-id {missing_devid} --dry-run "
        ">/tmp/out 2>/tmp/err"
    )
    assert status == 0, f"dry-run should succeed; exit {status}"
    out = machine.succeed("cat /tmp/out")
    err = machine.succeed("cat /tmp/err")
    assert (
        "[warn]  ENOSPC pre-flight check failed:" in out
    ), f"expected [warn] line on stdout; got:\n{out}"
    assert (
        "; proceeding anyway" in out
    ), f"expected canonical suffix on stdout; got:\n{out}"
    assert (
        "warning:" not in out
    ), f"dry-run stdout must not carry the legacy 'warning:' prefix; got:\n{out}"
    assert err.strip() == "", f"dry-run stderr must be empty; got:\n{err!r}"

# --- Phase 5: Real-run must emit the canonical `[warn]  ...` stderr line ---
# This phase mutates the pool (the remove-missing completes), so it must
# come last.

with subtest("real-run: warn appears on stderr with the canonical [warn] prefix"):
    (status, _) = run_with_wrapper(
        f"remove-missing --missing-id {missing_devid} --yes "
        ">/tmp/out2 2>/tmp/err2"
    )
    assert status == 0, f"real-run should succeed; exit {status}"
    err2 = machine.succeed("cat /tmp/err2")
    assert (
        "[warn]  ENOSPC pre-flight check failed:" in err2
    ), f"expected canonical '[warn]  ...' line on stderr; got:\n{err2}"
    assert (
        "; proceeding anyway" in err2
    ), f"expected canonical suffix on stderr; got:\n{err2}"
    assert (
        "warning:" not in err2
    ), f"real-run stderr must not carry the legacy 'warning:' prefix; got:\n{err2}"

with subtest("real-run actually removed the missing device"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert (
        "missing" not in fi_show.lower()
    ), f"pool should have no missing device after real-run:\n{fi_show}"

machine.shutdown()
