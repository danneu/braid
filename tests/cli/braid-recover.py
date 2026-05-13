# Test: braid recover
#
# Intent: Verify `braid recover` can self-mount the pool (open LUKS + mount)
# and rebuild pool.json from live state when recovering from an interrupted
# mutation.
#
# Why it exists: There was a chicken-and-egg: `braid unlock` refuses when
# pending-op.json exists, and `braid recover` required the pool to already be
# mounted. Users had to manually run cryptsetup + mount — the exact low-level
# commands braid exists to abstract away.
#
# Scenario: 2-disk RAID1 pool is created, test data written, pool locked.
# A pending-op.json is injected to simulate an interrupted add of a third disk.
# Tests exercise: unlock blocked by journal, recover self-mounts and rebuilds,
# data intact, normal operations resume.

import json
import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
pq = shlex.quote(passphrase)
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add_cmd(key):
    return (
        f"printf '%s\\n' {pq} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 {key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes"
    )


def post_add_balance_op():
    return {
        "op": "Add",
        "phase": "PostAddBalanceRaid1",
        "targets": {},
    }


def has_member(pool, name):
    return any(member["name"] == name for member in pool["disks"].values())


# --- Phase 1: Build 2-disk RAID1 pool and write test data ---

with subtest("Build 2-disk pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed("mountpoint -q /mnt/storage")

    # Write test data
    machine.succeed("echo 'recovery-test-data' > /mnt/storage/testfile.txt")
    machine.succeed("sync")

    # Capture pool.json for journal construction
    pool_json = json.loads(machine.succeed("cat /var/lib/braid/pool.json"))

# --- Phase 2: Lock pool and inject pending-op.json ---

with subtest("Lock pool and inject journal"):
    machine.succeed("braid lock")
    machine.fail("mountpoint -q /mnt/storage")

    # Build a pending-op.json simulating an interrupted add after pool
    # membership was already committed and only post-add maintenance remains.
    journal = {
        "started_at": "2026-01-01T00:00:00Z",
        "op": post_add_balance_op(),
        "pre_membership": pool_json,
        "target_membership": pool_json,
    }
    journal_json = json.dumps(journal)
    machine.succeed(
        f"cat > /var/lib/braid/pending-op.json << 'JOURNAL_EOF'\n"
        f"{journal_json}\n"
        f"JOURNAL_EOF"
    )
    machine.succeed("test -f /var/lib/braid/pending-op.json")

# --- Phase 3: braid unlock must fail ---

with subtest("braid unlock refuses with journal present"):
    exit_code, output = machine.execute(
        f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin 2>&1"
    )
    assert exit_code != 0, f"unlock should fail, but got exit {exit_code}"
    assert "interrupted operation" in output, (
        f"Expected 'interrupted operation' in output, got: {output}"
    )

# --- Phase 3.5: dry-run contracts for `braid recover` (PR 6) ---
#
# Intent: pin the stdout/stderr split of `braid recover --dry-run`.
# Successful dry-run must print exactly one rendered Preview to stdout
# (entry banner, probe notes, step block) with stderr empty. Preview-
# generation failures must print accumulated context + error to stderr
# and keep stdout empty.
#
# Why it exists: PR 6 migrates recover to the shared `Preview` /
# plan-object shape. Without subprocess tests, a regression that leaked
# probe lines to stderr on success, routed the entry banner back to
# stderr in dry-run, dropped state-recovery steps, or dropped the
# preserved-context notes on the failure path would pass the Rust unit
# tests (which don't cross the stdout/stderr boundary) while breaking
# the new wire contract.

# --- Test 3a: dry-run preserved-context failure (DegradedRefused) ---
#
# Scenario: re-inject a journal whose target_membership includes a
# third disk (disk3) that is not plugged in. With no --allow-degraded,
# plan_open_pool accumulates per-disk probe events (disk1+disk2
# available, disk3 absent) and then returns DegradedRefused. cmd_recover
# must render those accumulated notes to stderr before the refusal
# message, with stdout empty and a nonzero exit code.
with subtest("Test 3a: dry-run preserved-context failure -> stdout empty, stderr has context"):
    # Build a journal whose target_membership contains disk3 in
    # addition to the existing disk1+disk2. disk3 has no virtio-disk3
    # device (nothing plugged), so plan_open_pool emits DiskAbsent for
    # it and then refuses the degraded mount.
    target_with_disk3 = {
        "disks": {
            **pool_json["disks"],
            "33333333-3333-3333-3333-333333333333": {
                "name": "disk3",
                "by_id": "/dev/disk/by-id/virtio-disk3",
            },
        },
    }
    journal_deg = {
        "started_at": "2026-01-01T00:00:00Z",
        "op": post_add_balance_op(),
        "pre_membership": pool_json,
        "target_membership": target_with_disk3,
    }
    machine.succeed(
        f"cat > /var/lib/braid/pending-op.json << 'JOURNAL_EOF'\n"
        f"{json.dumps(journal_deg)}\n"
        f"JOURNAL_EOF"
    )

    exit_code, _ = machine.execute(
        "braid recover --dry-run >/tmp/pcf-stdout 2>/tmp/pcf-stderr"
    )
    assert exit_code != 0, f"recover --dry-run should refuse without --allow-degraded, got exit {exit_code}"

    out = machine.succeed("cat /tmp/pcf-stdout")
    err = machine.succeed("cat /tmp/pcf-stderr")

    assert out == "", (
        "stdout must be empty on preview-generation failure; got: {!r}".format(out)
    )
    assert "Recovering from interrupted" in err, (
        "stderr must contain the entry banner before the error; got: {!r}".format(err)
    )
    # Probe notes for the two present disks appear before the refusal.
    for name in ("disk1", "disk2"):
        marker = "[ok]   disk {}".format(name)
        assert marker in err, (
            "stderr must contain probe note {!r}; got: {!r}".format(marker, err)
        )
    assert "[skip] disk disk3" in err, (
        "stderr must contain the absent-disk skip note; got: {!r}".format(err)
    )
    assert "\x1b[" not in err, (
        "stderr must be plain without a TTY; got: {!r}".format(err)
    )
    # Banner + per-disk notes precede the refusal message.
    banner_pos = err.find("Recovering from interrupted")
    disk3_pos = err.find("[skip] disk disk3")
    refusal_pos = err.find("refusing to mount")
    assert banner_pos != -1 and disk3_pos != -1 and refusal_pos != -1, (
        "expected banner, skip note, and refusal markers in stderr; got: {!r}".format(err)
    )
    assert banner_pos < disk3_pos < refusal_pos, (
        "expected banner < skip note < refusal in stderr; got: {!r}".format(err)
    )

    # Restore the simple journal for the following subtests.
    journal_json = json.dumps(journal)
    machine.succeed(
        f"cat > /var/lib/braid/pending-op.json << 'JOURNAL_EOF'\n"
        f"{journal_json}\n"
        f"JOURNAL_EOF"
    )

# --- Test 3b: dry-run stepful success when pool not mounted ---
#
# Scenario: pool is locked (from Phase 2) and the simple journal is
# present. `braid recover --dry-run` must print the entry banner, the
# per-disk probe notes, and the full step block (LUKS open, btrfs
# device scan, mount, write pool.json, clear pending-op.json) to
# stdout. stderr must be exactly empty.
with subtest("Test 3b: dry-run stepful not-mounted -> stdout has banner+notes+steps, stderr empty"):
    machine.fail("mountpoint -q /mnt/storage")

    machine.succeed(
        "braid recover --dry-run >/tmp/drn-stdout 2>/tmp/drn-stderr"
    )
    out = machine.succeed("cat /tmp/drn-stdout")
    err = machine.succeed("cat /tmp/drn-stderr")

    assert err == "", (
        "stderr must be empty on dry-run success; got: {!r}".format(err)
    )
    assert "Recovering from interrupted" in out, (
        "stdout must contain the entry banner; got: {!r}".format(out)
    )
    for name in ("disk1", "disk2"):
        marker = "[ok]   disk {}".format(name)
        assert marker in out, (
            "stdout must contain probe note {!r}; got: {!r}".format(marker, out)
        )
    assert "\x1b[" not in out, (
        "stdout must be plain without a TTY; got: {!r}".format(out)
    )
    assert "LUKS open" in out, (
        "stdout must contain LUKS open step; got: {!r}".format(out)
    )
    assert "btrfs device scan" in out, (
        "stdout must contain btrfs device scan step; got: {!r}".format(out)
    )
    assert "write recovered pool.json" in out, (
        "stdout must contain write pool.json step; got: {!r}".format(out)
    )
    assert "clear pending-op.json" in out, (
        "stdout must contain clear pending-op.json step; got: {!r}".format(out)
    )
    banner_pos = out.find("Recovering from interrupted")
    probe_pos = out.find("[ok]   disk disk1")
    scan_pos = out.find("btrfs device scan")
    write_pos = out.find("write recovered pool.json")
    assert banner_pos < probe_pos < scan_pos < write_pos, (
        "expected banner < probe notes < scan/steps < write step; got: {!r}".format(out)
    )

    # Dry-run is read-only: pool is still locked.
    machine.fail("mountpoint -q /mnt/storage")

# --- Test 3c: no-journal failure (no-context) ---
#
# Scenario: temporarily move pending-op.json aside. `braid recover
# --dry-run` has nothing to recover; the failure happens before any
# probe context accumulates. stdout must be empty and stderr must
# contain only the error message.
with subtest("Test 3c: no-journal failure -> stdout empty, stderr has only the error"):
    machine.succeed("mv /var/lib/braid/pending-op.json /tmp/saved-pending-op.json")

    exit_code, _ = machine.execute(
        "braid recover --dry-run >/tmp/nj-stdout 2>/tmp/nj-stderr"
    )
    assert exit_code != 0, f"recover --dry-run with no journal should fail, got exit {exit_code}"

    out = machine.succeed("cat /tmp/nj-stdout")
    err = machine.succeed("cat /tmp/nj-stderr")

    assert out == "", (
        "stdout must be empty on no-context failure; got: {!r}".format(out)
    )
    assert "no pending operation journal found -- nothing to recover" in err, (
        "stderr must name the missing-journal condition; got: {!r}".format(err)
    )
    assert "Recovering from interrupted" not in err, (
        "stderr must not contain the entry banner when no journal is loaded; got: {!r}".format(err)
    )

    # Restore the simple journal for the remaining real-run subtest.
    machine.succeed("mv /tmp/saved-pending-op.json /var/lib/braid/pending-op.json")

# --- Phase 4: braid recover self-mounts and recovers ---

with subtest("braid recover self-mounts and rebuilds pool.json"):
    machine.succeed(
        f"printf '%s\\n' {pq} | braid recover --passphrase-stdin "
        f">/tmp/recover.out 2>/tmp/recover.err"
    )
    err = machine.succeed("cat /tmp/recover.err")
    wait_line = "[wait] passphrase: checking against disk1...\n"
    unlocked_line = "[ok]   disk disk1: unlocked\n"
    assert wait_line in err, (
        f"expected recover passphrase wait line, got: {err!r}"
    )
    assert err.find(wait_line) < err.find(unlocked_line), (
        f"wait line must precede first unlocked row, got: {err!r}"
    )
    unlocking_wait = "[wait] disk disk1: unlocking...\n"
    mounting_wait = "[wait] pool: mounting /mnt/storage...\n"
    mounted_line = "[ok]   pool: mounted /mnt/storage\n"
    assert unlocking_wait in err, (
        f"per-disk unlocking wait row missing, got: {err!r}"
    )
    assert err.find(unlocking_wait) < err.find(unlocked_line), (
        f"unlocking wait must precede unlocked row, got: {err!r}"
    )
    assert mounting_wait in err, (
        f"pool mounting wait row missing, got: {err!r}"
    )
    assert err.find(mounting_wait) < err.find(mounted_line), (
        f"mounting wait must precede mounted row, got: {err!r}"
    )

    # This journal is Add::PostAddBalanceRaid1: membership already committed
    # and only post-add maintenance remains. The relock/remount cycle is
    # replace-specific, so this add recovery should not take the pool offline
    # a second time before replaying the owed balance.
    assert "recover remount cycle" not in err, (
        f"post-add recovery must not run the replace remount cycle, got: {err!r}"
    )
    # post-{label} RAID1 soft balance replay rows from replay_post_mutation.
    # Pin the substring shared with module tests for cross-suite consistency.
    soft_replay_wait = "replaying post-add RAID1 soft balance"
    soft_replay_ok = "[ok]   pool: RAID1 soft balance replay complete\n"
    assert soft_replay_wait in err, (
        f"post-add soft balance replay wait row missing, got: {err!r}"
    )
    assert err.find(soft_replay_wait) < err.find(soft_replay_ok), (
        f"soft balance replay wait must precede ok row, got: {err!r}"
    )

    # Pool must be mounted
    machine.succeed("mountpoint -q /mnt/storage")

    # pool.json must exist and contain disk1 + disk2
    recovered = json.loads(machine.succeed("cat /var/lib/braid/pool.json"))
    assert has_member(recovered, "disk1"), f"disk1 missing from recovered pool.json: {recovered}"
    assert has_member(recovered, "disk2"), f"disk2 missing from recovered pool.json: {recovered}"

    # pending-op.json must be cleared
    machine.fail("test -f /var/lib/braid/pending-op.json")

# --- Test 4a: dry-run stepful already-mounted -> stdout has banner + AlreadyMounted + write/clear steps ---
#
# Scenario: pool is now mounted (from Phase 4) and the journal has been
# cleared. Re-inject the simple journal and run `braid recover
# --dry-run`. plan_open_pool returns None (already mounted), so the
# preview contains the entry banner + "pool already mounted at
# /mnt/storage" Info note + the write/clear state-recovery steps.
# stderr must stay empty because this is still a dry-run success.
with subtest("Test 4a: dry-run already-mounted stepful -> stdout has banner + AlreadyMounted + steps"):
    machine.succeed("mountpoint -q /mnt/storage")

    # Re-inject the simple journal.
    machine.succeed(
        f"cat > /var/lib/braid/pending-op.json << 'JOURNAL_EOF'\n"
        f"{journal_json}\n"
        f"JOURNAL_EOF"
    )

    machine.succeed(
        "braid recover --dry-run >/tmp/dra-stdout 2>/tmp/dra-stderr"
    )
    out = machine.succeed("cat /tmp/dra-stdout")
    err = machine.succeed("cat /tmp/dra-stderr")

    assert err == "", (
        "stderr must be empty on dry-run success; got: {!r}".format(err)
    )
    assert "Recovering from interrupted" in out, (
        "stdout must contain the entry banner; got: {!r}".format(out)
    )
    assert "pool already mounted at /mnt/storage" in out, (
        "stdout must contain the AlreadyMounted note; got: {!r}".format(out)
    )
    assert "write recovered pool.json" in out, (
        "stdout must contain write pool.json step; got: {!r}".format(out)
    )
    assert "clear pending-op.json" in out, (
        "stdout must contain clear pending-op.json step; got: {!r}".format(out)
    )
    # Stepful success must not emit the generic `nothing to do.` fallback.
    assert "nothing to do." not in out, (
        "stepful dry-run must not emit the `nothing to do.` fallback; got: {!r}".format(out)
    )
    # The already-mounted case also must not emit mount/LUKS-open steps.
    assert "LUKS open" not in out, (
        "already-mounted dry-run must not emit LUKS open steps; got: {!r}".format(out)
    )

    # Dry-run is read-only: journal must still be present.
    machine.succeed("test -f /var/lib/braid/pending-op.json")
    # Clear the journal so the remaining phases (data intact + normal
    # ops) see the clean post-recovery state.
    machine.succeed("rm /var/lib/braid/pending-op.json")

# --- Phase 5: Test data intact ---

with subtest("Test data intact after recovery"):
    content = machine.succeed("cat /mnt/storage/testfile.txt").strip()
    assert content == "recovery-test-data", f"Expected 'recovery-test-data', got: {content}"

# --- Phase 6: Normal operations resume ---

with subtest("Normal operations resume after recovery"):
    machine.succeed("braid lock")
    machine.fail("mountpoint -q /mnt/storage")

    machine.succeed(
        f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin"
    )
    machine.succeed("mountpoint -q /mnt/storage")

    # Data still there
    content = machine.succeed("cat /mnt/storage/testfile.txt").strip()
    assert content == "recovery-test-data", f"Expected 'recovery-test-data', got: {content}"

machine.shutdown()
