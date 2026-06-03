# Test: braid unlock
#
# Intent: Verify `braid unlock` opens LUKS volumes and mounts the btrfs pool
# in one idempotent command.
#
# Why it exists: After a NixOS rebuild or missed initrd unlock window, there is
# no CLI path to open LUKS volumes and mount the pool. Users must manually run
# cryptsetup open + btrfs device scan + mount. This test ensures `braid unlock`
# handles all the common scenarios correctly.
#
# Scenario: 3-disk RAID1 pool is set up via `braid add`, then everything is
# torn down (unmount + cryptsetup close). Tests exercise: happy path, idempotent
# re-run, partial state, missing disk (degraded), wrong passphrase, and
# uninitialized disk.

import json


def member_names(pool):
    return {member["name"] for member in pool["disks"].values()}


def member(pool, name):
    for entry in pool["disks"].values():
        if entry["name"] == name:
            return entry
    raise AssertionError(f"{name} missing from pool.json: {pool}")


def member_uuid(pool, name):
    for uuid, entry in pool["disks"].items():
        if entry["name"] == name:
            return uuid
    raise AssertionError(f"{name} missing from pool.json: {pool}")


import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"
DELAYED_DISKS = {"disk1", "disk2"}


def disk_path(key):
    if key in DELAYED_DISKS:
        return f"/dev/disk/by-id/braid-test-{key}-delay"
    return f"/dev/disk/by-id/virtio-{key}"


def add_cmd(key):
    """Build a `braid add <key> --yes` command."""
    pq = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {pq} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 {key}={disk_path(key)} --passphrase-stdin --yes"
    )


def unlock_cmd(passphrase_str=None, extra=""):
    """Build a `braid unlock` command."""
    if passphrase_str is not None:
        pq = shlex.quote(passphrase_str)
        return f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin {extra}"
    return f"braid unlock {extra}"


def close_all():
    """Unmount pool and close all LUKS mappers."""
    machine.execute("umount /mnt/storage 2>/dev/null || true")
    for k in ["disk1", "disk2", "disk3"]:
        machine.execute(f"cryptsetup close braid-{k} 2>/dev/null || true")


# --- Setup: Create a 3-disk RAID1 pool ---

with subtest("Setup: create 3-disk pool"):
    for name in DELAYED_DISKS:
        dm_delay_create(machine, name)
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed(add_cmd("disk3"))

    # Write test data
    machine.succeed("echo 'persistent data' > /mnt/storage/test.txt")
    machine.succeed("sync")

    # Tear everything down
    close_all()

    # Verify pool is gone
    machine.fail("mountpoint -q /mnt/storage")
    machine.fail("test -e /dev/mapper/braid-disk1")

# --- Test 1: Happy path ---

with subtest("Test 1: happy path — all locked, unlock opens everything"):
    machine.succeed(
        f"{unlock_cmd(passphrase)} >/tmp/probe-stdout 2>/tmp/probe-stderr"
    )
    probe_err = machine.succeed("cat /tmp/probe-stderr")
    assert "[ok]   disk disk1" in probe_err, (
        f"expected probe stderr rows, got: {probe_err!r}"
    )
    wait_line = "[wait] passphrase: checking against disk1...\n"
    accepted_line = "[ok]   passphrase: accepted by disk1\n"
    unlocked_line = "[ok]   disk disk1: unlocked\n"
    assert wait_line in probe_err, (
        f"expected credential verification wait line, got: {probe_err!r}"
    )
    assert accepted_line in probe_err, (
        f"expected passphrase accepted row, got: {probe_err!r}"
    )
    assert probe_err.find(wait_line) < probe_err.find(accepted_line), (
        f"passphrase wait must precede accepted row, got: {probe_err!r}"
    )
    assert probe_err.find(accepted_line) < probe_err.find(unlocked_line), (
        f"passphrase accepted row must precede first unlocked row, got: {probe_err!r}"
    )
    unlocking_wait = "[wait] disk disk1: unlocking...\n"
    mounting_wait = "[wait] pool: mounting /mnt/storage...\n"
    mounted_line = "[ok]   pool: mounted /mnt/storage\n"
    assert unlocking_wait in probe_err, (
        f"per-disk unlocking wait row missing, got: {probe_err!r}"
    )
    assert probe_err.find(unlocking_wait) < probe_err.find(unlocked_line), (
        f"unlocking wait must precede unlocked row, got: {probe_err!r}"
    )
    assert mounting_wait in probe_err, (
        f"pool mounting wait row missing, got: {probe_err!r}"
    )
    assert probe_err.find(mounting_wait) < probe_err.find(mounted_line), (
        f"mounting wait must precede mounted row, got: {probe_err!r}"
    )
    assert "\x1b[" not in probe_err, (
        f"unlock stderr must be plain without a TTY, got: {probe_err!r}"
    )

    # Pool mounted
    machine.succeed("mountpoint -q /mnt/storage")

    # All mappers open
    for k in ["disk1", "disk2", "disk3"]:
        machine.succeed(f"test -e /dev/mapper/braid-{k}")

    # Data intact
    content = machine.succeed("cat /mnt/storage/test.txt").strip()
    assert content == "persistent data", f"Expected 'persistent data', got '{content}'"

    # Mount options pinned by ADR 015: skip_balance, subvolid=5, no compression
    opts = machine.succeed("findmnt -o OPTIONS -n /mnt/storage").strip()
    assert "skip_balance" in opts, f"Expected skip_balance in mount options, got: {opts}"
    assert "subvolid=5" in opts, f"Expected subvolid=5 in mount options, got: {opts}"
    assert "compress" not in opts, (
        f"Expected no compression option in mount options "
        f"(see ADR 015), got: {opts}"
    )

# --- Test 2: Idempotent ---

with subtest("Test 2: idempotent — unlock again is a no-op"):
    machine.succeed(unlock_cmd(passphrase))

    # Still mounted, still works
    machine.succeed("mountpoint -q /mnt/storage")
    content = machine.succeed("cat /mnt/storage/test.txt").strip()
    assert content == "persistent data", f"Expected 'persistent data', got '{content}'"

# --- Test 2c: already-mounted unlock: exit 0, stderr message, no remount ---
#
# Intent: Verify that `braid unlock` against an already-mounted pool exits 0,
# emits "pool already mounted at /mnt/storage" to stderr (not stdout),
# performs no cryptsetup/mount work, and leaves the pool unchanged.
#
# Why it exists: The wrapper's pre-CLI `mountpoint -q` short-circuit was
# removed in favor of the CLI's own check inside `plan_open_pool`. Without
# this test, a regression could silently re-route the message back to
# stdout, drop it entirely, or drop the `Ok(None)` short-circuit and cause
# redundant mount work — all exit-0 and all invisible to the existing
# idempotent test (which asserts data integrity, not control flow).
#
# Scenario: Pool is already mounted from Tests 1 and 2. Run `braid unlock`
# with stdout and stderr captured separately and assert the shape.
with subtest("Test 2c: already-mounted unlock -> exit 0, stderr message, no remount"):
    # Precondition: pool is mounted from Tests 1/2.
    machine.succeed("mountpoint -q /mnt/storage")

    # Snapshot mount source and mapper set to prove no remount/reopen work.
    before_src = machine.succeed("findmnt -n -o SOURCE /mnt/storage").strip()
    before_mappers = machine.succeed(
        "ls /dev/mapper/ | grep '^braid-' | sort"
    ).strip()

    # Run with stdout/stderr split. machine.succeed asserts exit 0.
    machine.succeed(
        f"{unlock_cmd(passphrase)} >/tmp/amm-stdout 2>/tmp/amm-stderr"
    )
    out = machine.succeed("cat /tmp/amm-stdout")
    err = machine.succeed("cat /tmp/amm-stderr")

    # Message is on stderr, absent from stdout.
    assert "pool already mounted" in err, (
        "expected 'pool already mounted' on stderr; "
        "stderr={!r} stdout={!r}".format(err, out)
    )
    assert "pool already mounted" not in out, (
        "message leaked to stdout; stdout={!r}".format(out)
    )

    # No remount (same mount source) and same mapper set.
    after_src = machine.succeed("findmnt -n -o SOURCE /mnt/storage").strip()
    after_mappers = machine.succeed(
        "ls /dev/mapper/ | grep '^braid-' | sort"
    ).strip()
    assert before_src == after_src, (
        "mount source changed: before={} after={}".format(before_src, after_src)
    )
    assert before_mappers == after_mappers, (
        "mapper set changed: before={!r} after={!r}".format(
            before_mappers, after_mappers
        )
    )

# --- Test 2d: dry-run already-mounted -> note-only success on stdout ---
#
# Intent: `braid unlock --dry-run` against an already-mounted pool prints
# exactly `pool already mounted at /mnt/storage\n` to stdout, keeps stderr
# empty, and does not emit any step lines.
#
# Why it exists: PR 5 routes unlock through the shared Preview model. The
# note-only success path (AlreadyMounted -> Info note + zero steps) is the
# guardrail against silent regression of the new "notes with zero steps"
# dry-run contract. Without this subtest, a regression that reintroduced a
# trailing `nothing to do.` line, leaked step lines onto stdout, or routed
# the message back to stderr would still pass Test 2c (which only pins the
# real-run channel for the already-mounted case).
#
# Scenario: pool is mounted from Tests 1/2/2c. Invoke `braid unlock --dry-run`
# with stdout and stderr captured separately.
with subtest("Test 2d: dry-run already-mounted -> stdout Info note, stderr empty"):
    machine.succeed("mountpoint -q /mnt/storage")

    machine.succeed(
        "braid unlock --dry-run >/tmp/dam-stdout 2>/tmp/dam-stderr"
    )
    out = machine.succeed("cat /tmp/dam-stdout")
    err = machine.succeed("cat /tmp/dam-stderr")

    assert out == "pool already mounted at /mnt/storage\n", (
        "stdout must be exactly the Info note; got: {!r}".format(out)
    )
    assert err == "", (
        "stderr must be empty on dry-run success; got: {!r}".format(err)
    )
    # Belt-and-suspenders: no step lines leaked onto stdout.
    assert "LUKS open" not in out, (
        "step lines must not appear in a note-only dry-run; got: {!r}".format(out)
    )
    assert "btrfs device scan" not in out, (
        "step lines must not appear in a note-only dry-run; got: {!r}".format(out)
    )

# --- Test 2e: dry-run stepful success -> probe notes + step block on stdout ---
#
# Intent: `braid unlock --dry-run` against a locked pool prints bracketed
# per-disk probe notes followed by the LUKS open / btrfs scan / mount step
# block to stdout, with stderr exactly empty and no side effects.
#
# Why it exists: PR 5 requires dry-run stepful success to include probe
# notes on stdout BEFORE the open/scan/mount steps. A regression that
# dropped the probe notes, leaked them to stderr, or reordered them after
# the step block would break the new "probe context precedes steps" dry-run
# contract.
#
# Scenario: close the pool so it is fully locked, then run
# `braid unlock --dry-run` with stdout and stderr captured separately.
with subtest("Test 2e: dry-run stepful -> stdout has probe + steps, stderr empty"):
    close_all()
    machine.fail("mountpoint -q /mnt/storage")

    machine.succeed(
        "braid unlock --dry-run >/tmp/dru-stdout 2>/tmp/dru-stderr"
    )
    out = machine.succeed("cat /tmp/dru-stdout")
    err = machine.succeed("cat /tmp/dru-stderr")

    assert err == "", (
        "stderr must be empty on dry-run success; got: {!r}".format(err)
    )
    for name in ("disk1", "disk2", "disk3"):
        marker = "[ok]   disk {}".format(name)
        assert marker in out, (
            "expected probe note {!r} on stdout; got: {!r}".format(marker, out)
        )
    assert "LUKS open" in out, (
        "expected LUKS open step on stdout; got: {!r}".format(out)
    )
    assert "btrfs device scan" in out, (
        "expected btrfs device scan step on stdout; got: {!r}".format(out)
    )

    probe_pos = out.find("[ok]   disk disk1")
    scan_pos = out.find("btrfs device scan")
    assert probe_pos != -1 and probe_pos < scan_pos, (
        "probe notes must render before the step block; got: {!r}".format(out)
    )

    # Dry-run is read-only: pool is still locked and no mapper exists.
    machine.fail("mountpoint -q /mnt/storage")
    machine.fail("test -e /dev/mapper/braid-disk1")

# --- Test 2b: Unlock enriches pool.json ---

with subtest("Test 2b: unlock enriches pool.json with runtime metadata"):
    close_all()

    # Unlock should succeed and enrich pool.json with runtime fields
    machine.succeed(unlock_cmd(passphrase))
    machine.succeed("mountpoint -q /mnt/storage")

    # Verify pool.json has UUID-keyed entries and runtime metadata for all 3 disks
    pool_raw = machine.succeed("cat /var/lib/braid/pool.json")
    pool_m = json.loads(pool_raw)

    assert member_names(pool_m) == {"disk1", "disk2", "disk3"}, \
        f"Expected 3 disks in pool.json, got: {member_names(pool_m)}"

    for name in ["disk1", "disk2", "disk3"]:
        entry = member(pool_m, name)
        assert member_uuid(pool_m, name), \
            f"{name} UUID key should not be empty after unlock: {pool_m}"
        assert entry.get("devid") is not None, \
            f"{name}.devid should not be None after unlock: {entry}"

# --- Test 3: Partial state ---

with subtest("Test 3: partial state — one mapper closed, pool unmounted"):
    # Close just disk1 and unmount
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk1")

    # disk2 and disk3 still open
    machine.succeed("test -e /dev/mapper/braid-disk2")
    machine.succeed("test -e /dev/mapper/braid-disk3")

    # Unlock should reopen disk1 and remount
    machine.succeed(unlock_cmd(passphrase))

    machine.succeed("mountpoint -q /mnt/storage")
    machine.succeed("test -e /dev/mapper/braid-disk1")
    content = machine.succeed("cat /mnt/storage/test.txt").strip()
    assert content == "persistent data", f"Expected 'persistent data', got '{content}'"

# --- Test 4a: Missing disk -- refuses degraded by default ---
#
# Strengthened for PR 5: capture stdout and stderr separately and pin the
# preserved-context contract. Accumulated per-disk probe notes render to
# stderr BEFORE the refusal message; stdout stays empty on the failure
# path. A regression that leaked the refusal onto stdout, dropped the
# probe context, or reordered it after the refusal would break the Shape A
# contract for unlock.
with subtest("Test 4a: missing disk -- refuses degraded, probe context precedes refusal on stderr"):
    close_all()

    # Remove disk3's by-id symlink to simulate unplugged disk
    machine.succeed("rm -f /dev/disk/by-id/virtio-disk3")

    ret = machine.execute(
        f"{unlock_cmd(passphrase)} >/tmp/r4a-stdout 2>/tmp/r4a-stderr"
    )
    out = machine.succeed("cat /tmp/r4a-stdout")
    err = machine.succeed("cat /tmp/r4a-stderr")

    assert ret[0] == 2, f"expected exit 2 for degraded refusal, got {ret[0]}"
    assert out == "", (
        "stdout must be empty on failure path; got: {!r}".format(out)
    )
    assert "refusing to mount degraded" in err, (
        "expected refusal message on stderr; got: {!r}".format(err)
    )
    assert "--allow-degraded" in err, (
        "expected --allow-degraded hint on stderr; got: {!r}".format(err)
    )

    # Probe notes must precede the refusal on stderr (Shape A contract).
    probe_marker = "[skip] disk disk3"
    refusal_marker = "refusing to mount degraded"
    probe_pos = err.find(probe_marker)
    refusal_pos = err.find(refusal_marker)
    assert probe_pos != -1, (
        "expected bracketed probe note for absent disk3 on stderr; got: {!r}".format(err)
    )
    assert probe_pos < refusal_pos, (
        "probe notes must render before the refusal on stderr; got: {!r}".format(err)
    )

    machine.fail("mountpoint -q /mnt/storage")

# --- Test 4a_dry: dry-run degraded refusal matches real-run contract ---
#
# Intent: `braid unlock --dry-run` against a pool with a missing member and
# no `--allow-degraded` must exit nonzero with stdout empty and stderr
# showing the accumulated probe notes followed by the refusal message.
#
# Why it exists: PR 5's failure-with-preserved-context contract applies to
# both real-run and dry-run. Without this subtest, a regression where the
# dry-run path skipped the note-to-stderr render (e.g. because the planner
# preserved notes only on the success branch) would slip through while the
# real-run Test 4a kept passing.
#
# Scenario: reuse Test 4a's teardown (pool closed, disk3 symlink gone).
with subtest("Test 4a_dry: dry-run missing disk -- stderr has probe + refusal, stdout empty"):
    # Idempotent: Test 4a left this state; redo to keep the test
    # self-contained if subtests are reordered.
    close_all()
    machine.succeed("rm -f /dev/disk/by-id/virtio-disk3")

    ret = machine.execute(
        "braid unlock --dry-run >/tmp/d4a-stdout 2>/tmp/d4a-stderr"
    )
    out = machine.succeed("cat /tmp/d4a-stdout")
    err = machine.succeed("cat /tmp/d4a-stderr")

    assert ret[0] == 2, f"expected exit 2 for dry-run degraded refusal, got {ret[0]}"
    assert out == "", (
        "stdout must be empty on dry-run failure; got: {!r}".format(out)
    )
    assert "refusing to mount degraded" in err, (
        "expected refusal message on stderr; got: {!r}".format(err)
    )

    probe_marker = "[skip] disk disk3"
    refusal_marker = "refusing to mount degraded"
    probe_pos = err.find(probe_marker)
    refusal_pos = err.find(refusal_marker)
    assert probe_pos != -1, (
        "expected bracketed probe note for absent disk3 on stderr; got: {!r}".format(err)
    )
    assert probe_pos < refusal_pos, (
        "probe notes must render before refusal on stderr; got: {!r}".format(err)
    )

    machine.fail("mountpoint -q /mnt/storage")

# --- Test 4b: Missing disk — --allow-degraded mounts degraded ---

with subtest("Test 4b: missing disk — --allow-degraded mounts degraded"):
    machine.succeed(
        f"{unlock_cmd(passphrase, extra='--allow-degraded')}"
        " >/tmp/unlock-degraded-stdout 2>/tmp/unlock-degraded-stderr"
    )
    err = machine.succeed("cat /tmp/unlock-degraded-stderr")
    assert "redundancy is reduced" in err, (
        f"expected post-mount degraded warning on stderr; got: {err!r}"
    )

    machine.succeed("mountpoint -q /mnt/storage")

    # disk1 and disk2 open, disk3 absent
    machine.succeed("test -e /dev/mapper/braid-disk1")
    machine.succeed("test -e /dev/mapper/braid-disk2")

    # Data intact (RAID1 redundancy)
    content = machine.succeed("cat /mnt/storage/test.txt").strip()
    assert content == "persistent data", f"Expected 'persistent data', got '{content}'"

    # Restore symlink for subsequent tests
    close_all()
    # The virtio symlinks are managed by udev; trigger a rescan
    machine.succeed("udevadm trigger && udevadm settle")
    machine.succeed("test -e /dev/disk/by-id/virtio-disk3")

# --- Test 6: Wrong passphrase ---

with subtest("Test 6: wrong passphrase rejected"):
    close_all()

    ret = machine.execute(unlock_cmd("wrongpassphrase"))
    assert ret[0] == 1, f"expected exit 1 for wrong passphrase, got {ret[0]}"

    # No mappers should have been opened
    machine.fail("test -e /dev/mapper/braid-disk1")

# --- Test 7: Uninitialized disk ---

# Intent: a raw (never-LUKS-formatted) disk listed in pool.json must be
#   detected at unlock time and surfaced through the structured
#   DegradedRefused error path with per-disk reason text.
# Why it exists: previously the per-disk status line said "LUKS header
#   damaged" (wrong vocabulary — the cryptsetup probe failed to read the
#   header at all, so it is "unreadable" in the canonical luks.rs sense)
#   and the final error was a generic "pool has missing devices" string
#   that did not name the disk or the cause. The new structured error
#   names each missing disk with its reason in probe order.
# Scenario: a 2-disk pool.json mixes one valid LUKS member (disk1, which
#   was braid add'd during setup with the test passphrase) and one
#   raw/unreadable member ('raw' pointing at virtio-disk4). disk1's
#   mapper is closed by close_all() above, so plan_open_pool classifies
#   disk1 as PresentLuks (closed) → goes into to_unlock, and raw as
#   PresentNotLuks → adds to missing. to_unlock is non-empty, so the
#   "no unlockable disks found" early return is skipped, and the
#   degraded check fires deterministically.
with subtest("Test 7: uninitialized disk detected — degraded-refused enumerates per-disk reasons"):
    close_all()

    # Save original pool.json for restoration
    original_pool = machine.succeed("cat /var/lib/braid/pool.json")

    # Two-disk pool: disk1 is real (already LUKS-formatted), 'raw' is
    # virtio-disk4 which has never been braid add'd.
    original = json.loads(original_pool)
    disk1_uuid = member_uuid(original, "disk1")
    mixed_pool = json.dumps({
        "disks": {
            disk1_uuid: {
                "name": "disk1",
                "by_id": disk_path("disk1"),
            },
            "44444444-4444-4444-4444-444444444444": {
                "name": "raw",
                "by_id": "/dev/disk/by-id/virtio-disk4",
            },
        },
    })
    machine.succeed(f"echo '{mixed_pool}' > /var/lib/braid/pool.json")

    # Redirect stderr to stdout so we can capture the error message
    cmd = unlock_cmd(passphrase) + " 2>&1"
    ret = machine.execute(cmd)
    assert ret[0] == 2, f"expected exit 2 for raw member in pool, got: {ret}"
    output = ret[1]

    # Deterministic: must reach the structured DegradedRefused path.
    assert "refusing to mount degraded" in output, \
        f"Expected DegradedRefused path, got: {output}"
    assert "raw: LUKS header unreadable" in output, \
        f"Expected per-disk reason 'raw: LUKS header unreadable', got: {output}"
    assert "braid unlock --allow-degraded" in output, \
        f"Expected --allow-degraded hint, got: {output}"

    # Composition pin: the live plan_open_pool probe must classify the raw member
    # so that format_degraded_refused renders BOTH the "raw: LUKS header unreadable"
    # line AND the doctor footer in the same message. The pure-function tests cover
    # the footer gate in isolation; this is the only check that the live per-disk
    # reason text and the is_luks_header_state() footer gate stay in agreement (a
    # future MissingReason variant reusing the per-disk text but not the gate, or
    # the footer moved out of the pure function, would slip past every other test).
    assert "run 'braid doctor' for recovery guidance" in output, \
        f"Expected doctor-recovery footer on unreadable-header degraded-refusal, got: {output}"

    # `plan_open_pool` only emits "LUKS header unreadable" for a non-LUKS
    # member; the "LUKS header damaged" vocabulary was removed entirely, so it
    # must never appear.
    assert "LUKS header damaged" not in output, \
        f"removed 'LUKS header damaged' string must not appear: {output}"

    # Cross-command negative invariant: unlock errors never point users at
    # local /var/lib/braid/luks-headers/ files (those are off-system).
    assert "/var/lib/braid/luks-headers/" not in output, \
        f"degraded-refused must not reference local backup directory: {output}"
    assert ".luksheader" not in output, \
        f"degraded-refused must not reference local .luksheader files: {output}"

    # Restore original pool.json
    machine.succeed(f"echo '{original_pool}' > /var/lib/braid/pool.json")

    # Defensive cleanup: today plan_open_pool returns DegradedRefused
    # before any cryptsetup open call, so disk1's mapper is never opened.
    # If a future refactor reorders things, this keeps Test 8 from
    # inheriting an open mapper.
    close_all()

# --- Test 8: Paused balance survives unlock (skip_balance) ---

with subtest("Test 8: paused balance survives unlock"):
    close_all()
    machine.succeed(unlock_cmd(passphrase))

    # Write enough data to create multiple btrfs chunks so balance has
    # observable work that can be paused mid-operation.
    machine.succeed(
        "dd if=/dev/urandom of=/mnt/storage/balancedata bs=1M count=32"
    )
    machine.succeed("sync")
    dm_delay_activate(machine, DELAYED_DISKS, write_delay_ms=500)

    import re

    # Bounded retry: start balance → pause → check for remaining work.
    # If the balance completes before pause catches it, restart with
    # the opposite conversion target so there's always new work to do.
    #
    # The start+pause loop runs in a single shell command to avoid the
    # serial-console overhead of machine.execute() — each roundtrip
    # takes ~100ms which is too slow for a balance that finishes in <1s.
    targets = ["single", "raid1"]
    paused_status = None
    for attempt in range(3):
        target = targets[attempt % 2]

        # Start balance in background, then tight-loop pause attempts
        # natively on the VM (no Python roundtrip overhead).
        machine.execute(
            f"btrfs balance start -dconvert={target} /mnt/storage "
            f"> /tmp/balance.log 2>&1 & "
            f"for i in $(seq 1 200); do "
            f"  btrfs balance pause /mnt/storage 2>/dev/null && break; "
            f"  sleep 0.02; "
            f"done"
        )

        # Check status from Python — one roundtrip is fine here.
        ret = machine.execute("btrfs balance status /mnt/storage")
        output = ret[1]
        lower = output.lower()

        if "paused" in lower:
            match = re.search(
                r"(\d+)\s+out of about\s+(\d+)\s+chunks", output
            )
            if match and int(match.group(1)) < int(match.group(2)):
                paused_status = output
                break

        # Balance completed or paused with no remaining work.
        # Clean up and retry with the opposite conversion target.
        machine.execute(
            "btrfs balance cancel /mnt/storage 2>/dev/null || true"
        )
        for _ in range(30):
            ret = machine.execute("btrfs balance status /mnt/storage")
            if "no balance" in ret[1].lower():
                break
            import time
            time.sleep(0.2)
        else:
            raise Exception(
                "Balance did not terminate after cancel — cannot retry safely"
            )
    else:
        raise Exception(
            "Could not pause balance with remaining work after 3 full attempts"
        )

    dm_delay_deactivate(machine, DELAYED_DISKS)

    # Lock and re-unlock
    close_all()
    ret = machine.execute(unlock_cmd(passphrase) + " 2>&1")
    assert ret[0] == 0, f"Unlock failed: {ret[1]}"

    # Balance must still be paused (not resumed by kernel).
    # Note: btrfs balance status can return exit code 1 for a paused
    # balance after remount, so check the text output, not the exit code.
    ret2 = machine.execute("btrfs balance status /mnt/storage")
    assert "paused" in ret2[1].lower(), \
        f"Expected balance still paused after unlock, got: {ret2[1]}"

    # Warning text must have been emitted
    assert "paused balance" in ret[1], \
        f"Expected paused balance warning, got: {ret[1]}"

    # Clean up: cancel the paused balance and remove test data
    machine.succeed("btrfs balance cancel /mnt/storage")
    machine.succeed("rm /mnt/storage/balancedata")

# --- Test 9: Failed post-open mount closes mappers opened by unlock ---

with subtest("Test 9: mount failure closes mappers opened by unlock"):
    close_all()

    # Destructive final subtest: wipe the btrfs signature inside disk1's LUKS
    # payload so unlock can open every mapper but cannot mount using disk1 as
    # the mount device.
    pq = shlex.quote(passphrase)
    machine.succeed(
        f"printf '%s' {pq} | cryptsetup luksOpen --key-file=- "
        f"{disk_path('disk1')} braid-disk1"
    )
    machine.succeed("wipefs -a /dev/mapper/braid-disk1")
    machine.succeed("cryptsetup close braid-disk1")

    ret = machine.execute(
        f"{unlock_cmd(passphrase)} >/tmp/fc-stdout 2>/tmp/fc-stderr"
    )
    out = machine.succeed("cat /tmp/fc-stdout")
    err = machine.succeed("cat /tmp/fc-stderr")

    assert ret[0] != 0, "expected non-zero exit for corrupted btrfs signature"
    assert out == "", "stdout must stay empty on mount failure; got: {!r}".format(out)
    assert "mount failed" in err, "expected mount failure on stderr; got: {!r}".format(err)
    assert "cleanup: closed LUKS mappers opened by this command." in err, (
        "expected cleanup success summary; got: {!r}".format(err)
    )
    assert "[wait] disk disk1: locking..." in err, (
        "expected cleanup lock wait row; got: {!r}".format(err)
    )

    machine.fail("mountpoint -q /mnt/storage")
    machine.fail("ls /dev/mapper/ | grep '^braid-'")

# NOTE on LUKS-header-corruption testing at the VM level:
#
# The new unlock error-enrichment path (verify/open-loop failure → probe
# header → emit off-system backup guidance) is NOT
# reachable via dd-based corruption in a VM test. The `plan_open_pool`
# probe phase runs `cryptsetup luksUUID` before the per-disk open loop
# ever starts; `luksUUID` validates enough of the LUKS2 header that any
# dd-based corruption reliably destroys it first. The disk gets
# classified as `ConfigDiskState::PresentNotLuks` and the pool fails
# with the existing "LUKS header unreadable" + degraded-refused path,
# which is out of scope for this PR.
#
# The enrichment path IS proven by unit tests in cli/src/mount.rs:
#
#   - `explain_open_failure_*` (4 tests): classify branches of the pure
#     helper (Unreadable/Ok/ProbeFailed).
#   - `unlock_*` (5 tests): drive `open_and_mount_pool` end-to-end with
#     MockRunner through all four enrichment call sites (keyfile/passphrase
#     × verify/open-loop) and the critical ProbeFailed-at-exit-2 case.
#
# The probe primitive itself (cli/src/luks.rs::probe_luks_header) is
# shared with `braid doctor` and validated at the VM level by
# tests/cli/braid-doctor.py, which wipes a real LUKS header and proves
# doctor's detection works end-to-end against real cryptsetup.

machine.shutdown()
