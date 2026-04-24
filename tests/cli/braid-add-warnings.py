# Test: braid add warning-routing fixtures (missing-devices).
#
# See braid-add-warnings.nix for scenario / intent / rationale.

import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add_cmd(key, extra=""):
    pq = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {pq} | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add {key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes {extra}"
    )


# --- Phase 0: build a 2-disk RAID1 pool ---

with subtest("Setup: build 2-disk RAID1 pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    df = machine.succeed("btrfs fi df /mnt/storage")
    assert "Data, RAID1" in df, f"Expected RAID1, got:\n{df}"

# --- Phase 1: synthesize a missing device ---

with subtest("Kill disk2: unmount, close mapper, remount degraded"):
    # Same fixture pattern used by replace-dead-disk.py. btrfs sees
    # disk2 as MISSING; probe_pool reports missing_count=1.
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk2")
    machine.succeed("mount -o degraded /dev/mapper/braid-disk1 /mnt/storage")
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "missing" in fi_show.lower(), f"Expected missing device:\n{fi_show}"

# --- Phase 2: dry-run -> stdout [warn] line, stderr empty ---

with subtest("Dry-run missing-device warning routes to stdout as [warn]"):
    # Intent: `braid add disk3 --dry-run` on a pool with a missing
    # device must surface the missing-devices diagnostic on stdout via
    # `[warn]  pool has 1 missing device...`, with stderr empty.
    # Why it exists: PR 7 migrated the legacy stderr eprintln! to a
    # `PreviewNote::Warn` body. A regression that left `warning:` baked
    # into the body would stack as `[warn]  warning: pool has ...`;
    # a regression that routed the note back to stderr would exit 0
    # and still slip past add-lifecycle coverage.
    # Scenario: 2-disk pool with 1 missing, operator starts planning an
    # add of disk3.
    machine.succeed(
        f"{add_cmd('disk3', '--dry-run')} >/tmp/md-stdout 2>/tmp/md-stderr"
    )
    out = machine.succeed("cat /tmp/md-stdout")
    err = machine.succeed("cat /tmp/md-stderr")

    assert "[warn]  pool has 1 missing device" in out, (
        "dry-run stdout must contain the `[warn]  pool has ...` body-only line;"
        " got: {!r}".format(out)
    )
    assert "warning:" not in out, (
        "dry-run note body must not carry the legacy `warning:` prefix; got: {!r}".format(out)
    )
    assert err == "", (
        "dry-run stderr must be empty on success; got: {!r}".format(err)
    )

# --- Phase 3: real-run -> stderr has exact legacy warning line ---

with subtest("Real-run missing-device warning stays on stderr with legacy prefix"):
    # Intent: `braid add disk3` (no --dry-run) with a missing device
    # must print the legacy `warning: pool has 1 missing device.
    # Consider repairing with `braid replace --missing-id <devid>`
    # first. Use `braid status` to see device IDs.` line on stderr
    # byte-identically -- the wording hasn't changed, only its
    # dispatch path.
    # Why it exists: log scrapers and user-facing docs pin this wording.
    # Regressions that dropped the `warning:` prefix, reworded the body,
    # or silently routed the line to stdout must fail here.
    # Scenario: same as Phase 2 but without --dry-run. The add may
    # proceed or fail depending on btrfs's degraded-mount tolerance;
    # the test only asserts the warning wiring, not the downstream
    # outcome.
    # Use execute so a downstream btrfs error does not abort the test
    # before we inspect stderr.
    ec = machine.execute(
        f"{add_cmd('disk3')} >/tmp/rmd-stdout 2>/tmp/rmd-stderr"
    )[0]
    err = machine.succeed("cat /tmp/rmd-stderr")

    expected_line = (
        "warning: pool has 1 missing device. Consider repairing with"
        " `braid replace --missing-id <devid>` first. Use `braid status`"
        " to see device IDs."
    )
    assert expected_line in err, (
        "real-run stderr must contain the exact legacy missing-devices warning line;"
        " exit={} stderr={!r}".format(ec, err)
    )

machine.shutdown()
