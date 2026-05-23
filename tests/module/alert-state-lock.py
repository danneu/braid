# Test: alert-state-lock
#
# Intent: Every command that writes alert state acquires
# /run/braid-pool.lock before it can read or mutate alert files.
#
# Why it exists: `braid monitor` and `braid ack` both read and write
# alert-latch.json / acked-stats.json around subprocess I/O. Without
# Rust-level serialization, owned by the pool lock, ack can clear an alert
# while monitor resurrects it from a stale snapshot. add/remove/remove-missing
# also write acked-stats.json and must share the same lock.
# See docs/design/decisions/026-pool-lock-rust-owned.md.
#
# Scenario: A monitor timer cycle, a manual ack, and pool membership
# commands contend with an already-running braid operation. The contended
# command must either skip harmlessly, wait briefly, or fail fast before
# any observable alert-state mutation.

import base64
import json


def member_names(pool):
    return {member["name"] for member in pool["disks"].values()}


def member(pool, name):
    for entry in pool["disks"].values():
        if entry["name"] == name:
            return entry
    raise AssertionError(f"{name} missing from pool.json: {pool}")
import shlex
import time


start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

passphrase = "testpassphrase"
pq = shlex.quote(passphrase)
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"

alert_latch_path = "/var/lib/braid/alert-latch.json"
alert_latch_corrupt_path = "/var/lib/braid/alert-latch.json.corrupt"
acked_stats_path = "/var/lib/braid/acked-stats.json"
absent = "__BRAID_TEST_ABSENT__"


def quote(value):
    return shlex.quote(str(value))


def write_file(path, contents):
    encoded = base64.b64encode(contents.encode()).decode()
    machine.succeed(
        "mkdir -p /var/lib/braid && "
        f"printf '%s' {quote(encoded)} | base64 -d > {quote(path)}"
    )


def read_file_or_absent(path):
    rc, out = machine.execute(
        f"if test -e {quote(path)}; then cat {quote(path)}; "
        f"else printf '%s' {quote(absent)}; fi"
    )
    assert rc == 0, f"failed to read {path}: {out}"
    return out


def assert_file_unchanged(path, before):
    after = read_file_or_absent(path)
    assert after == before, (
        f"{path} changed under held pool lock\n"
        f"before: {before!r}\nafter:  {after!r}"
    )


def acked_disk(missing_acked=False, read_io_errs=0):
    return {
        "missing_acked": missing_acked,
        "device_stats": {
            "read_io_errs": read_io_errs,
            "write_io_errs": 0,
            "flush_io_errs": 0,
            "corruption_errs": 0,
            "generation_errs": 0,
        },
    }


def write_acked_stats(entries):
    write_file(acked_stats_path, json.dumps(entries, sort_keys=True))


def write_missing_latch(devid):
    write_file(
        alert_latch_path,
        json.dumps({"causes": [{"type": "missing_device", "devid": devid}]}),
    )


def add_cmd(key):
    return (
        f"printf '%s\\n' {pq} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 {key}=/dev/disk/by-id/virtio-{key} "
        "--passphrase-stdin --yes"
    )


def run_with_timeout(command, seconds):
    return machine.execute(
        f"timeout {seconds} sh -c {quote(command)} 2>&1"
    )


def start_lock_holder():
    holder_pid = machine.succeed(
        "rm -f /tmp/holder.ready; "
        "nohup sh -c 'exec 9>/run/braid-pool.lock; "
        "flock -x 9; touch /tmp/holder.ready; sleep 60' "
        ">/dev/null 2>&1 & echo $!"
    ).strip()
    machine.wait_until_succeeds("test -e /tmp/holder.ready", timeout=10)
    locks = machine.succeed("cat /proc/locks")
    assert "FLOCK" in locks, (
        "no flock in /proc/locks after holder readiness signal:\n"
        f"{locks}"
    )
    return holder_pid


def stop_lock_holder(holder_pid):
    machine.execute(f"kill {quote(holder_pid)} 2>/dev/null || true")
    machine.execute("rm -f /tmp/holder.ready")


def start_lock_holder_until_release(release_path="/tmp/holder.release"):
    machine.succeed(f"rm -f /tmp/holder.ready {quote(release_path)}")
    holder_pid = machine.succeed(
        "nohup sh -c 'exec 9>/run/braid-pool.lock; "
        "flock -x 9; touch /tmp/holder.ready; "
        f"while [ ! -e {quote(release_path)} ]; do sleep 0.1; done' "
        ">/dev/null 2>&1 & echo $!"
    ).strip()
    machine.wait_until_succeeds("test -e /tmp/holder.ready", timeout=10)
    locks = machine.succeed("cat /proc/locks")
    assert "FLOCK" in locks, (
        "no flock in /proc/locks after holder readiness signal:\n"
        f"{locks}"
    )
    return holder_pid


def release_lock_holder(holder_pid, release_path="/tmp/holder.release"):
    machine.succeed(f"touch {quote(release_path)}")
    machine.execute(f"kill {quote(holder_pid)} 2>/dev/null || true")
    machine.execute(f"rm -f /tmp/holder.ready {quote(release_path)}")


def get_pool_devid(name):
    pool = json.loads(machine.succeed("cat /var/lib/braid/pool.json"))
    entry = member(pool, name)
    assert "devid" in entry, f"{name} has no devid in pool.json: {pool}"
    return int(entry["devid"])


def get_missing_devid():
    report = json.loads(machine.succeed("braid status --json"))
    devids = report.get("missing_devids", [])
    assert devids, "expected missing devids in braid status --json"
    return int(devids[0])


def btrfs_show_devids():
    show = machine.succeed("btrfs filesystem show /mnt/storage")
    devids = []
    for line in show.splitlines():
        tokens = line.split()
        if len(tokens) >= 2 and tokens[0] == "devid":
            devids.append(int(tokens[1]))
    assert devids, f"could not parse devids from btrfs show:\n{show}"
    return devids


def assert_contention_message(output):
    assert "another braid operation is already in progress" in output, (
        f"expected fail-fast contention message, got:\n{output}"
    )


with subtest("Setup: build 3-disk pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed(add_cmd("disk3"))
    machine.succeed("mountpoint -q /mnt/storage")
    fi_show = machine.succeed("btrfs filesystem show /mnt/storage")
    for mapper in ["braid-disk1", "braid-disk2", "braid-disk3"]:
        assert f"/dev/mapper/{mapper}" in fi_show, (
            f"{mapper} missing from pool:\n{fi_show}"
        )


# Intent: monitor exits 0 without touching alert-latch.json when the
# pool lock is already held.
# Why it exists: braid-monitor.service maps monitor rc=1 to alert beeper
# activation; a lock skip must not look like an active alert.
# Scenario: A timer fires while another braid command owns the pool lock.
with subtest("monitor skips silently while pool lock is held"):
    corrupt_bytes = "not json"
    write_file(alert_latch_path, corrupt_bytes)
    holder_pid = start_lock_holder()
    try:
        machine.succeed("systemctl start braid-monitor.service")
    finally:
        stop_lock_holder(holder_pid)

    assert read_file_or_absent(alert_latch_path) == corrupt_bytes, (
        "monitor rewrote alert-latch.json despite held pool lock"
    )
    machine.fail(f"test -e {quote(alert_latch_corrupt_path)}")
    alert_state = machine.succeed(
        "systemctl show -P ActiveState braid-alert.service"
    ).strip()
    assert alert_state == "inactive", (
        f"braid-alert.service should remain inactive, got {alert_state!r}"
    )
    journal = machine.succeed(
        "journalctl -u braid-monitor.service --no-pager"
    )
    assert "braid monitor failed" not in journal, journal
    assert "quarantining" not in journal, journal

    machine.succeed("systemctl start braid-monitor.service")
    assert read_file_or_absent(alert_latch_corrupt_path) == corrupt_bytes, (
        "monitor did not quarantine the corrupt latch after lock release"
    )
    machine.succeed("braid ack")
    machine.fail(f"test -e {quote(alert_latch_path)}")
    machine.fail(f"test -e {quote(alert_latch_corrupt_path)}")


# Intent: ack waits briefly for the pool lock, then fails without clearing
# alert-latch.json, rewriting acked-stats.json, or stopping the alert unit.
# Why it exists: ack is authoritative for baseline-and-clear; if it runs
# concurrently with monitor or pool mutation, state can be rolled back.
# Scenario: The user runs `braid ack` while another braid operation is
# still in flight.
with subtest("ack waits then fails without mutating alert state"):
    write_missing_latch(1)
    write_acked_stats({"1": acked_disk(False, 17)})
    latch_before = read_file_or_absent(alert_latch_path)
    acked_before = read_file_or_absent(acked_stats_path)
    machine.succeed("systemctl start braid-alert.service")

    holder_pid = start_lock_holder()
    start = time.monotonic()
    try:
        rc, out = run_with_timeout("braid ack", 15)
    finally:
        elapsed = time.monotonic() - start
        stop_lock_holder(holder_pid)

    assert rc == 1, f"expected ack contention exit 1, got rc={rc}; out={out}"
    assert elapsed >= 9, f"ack did not wait for lock timeout; elapsed={elapsed:.2f}s"
    assert elapsed <= 14, f"ack waited too long; elapsed={elapsed:.2f}s"
    assert "in progress" in out and "retry" in out, (
        f"expected bounded-wait retry message, got:\n{out}"
    )
    assert_file_unchanged(alert_latch_path, latch_before)
    assert_file_unchanged(acked_stats_path, acked_before)
    alert_state = machine.succeed("systemctl is-active braid-alert.service").strip()
    assert alert_state == "active", (
        f"ack should not have stopped braid-alert.service, got {alert_state!r}"
    )

    machine.succeed("braid ack")
    machine.fail("systemctl is-active --quiet braid-alert.service")


# Intent: ack waits while the pool lock is held, observes the holder's
# release mid-wait, re-acquires the lock within one poll interval, then
# clears the latch and stops the alert unit normally.
# Why it exists: protects the positive bounded-wait path at the VM
# seam. The existing ack contention subtest only covers the
# held-forever expiry path; its trailing `braid ack` runs after the
# holder is already stopped, so a regression in poll_acquire's retry
# shape -- e.g. a loop that sleeps the full timeout before its single
# re-attempt -- would still pass that subtest (elapsed ~10 s, rc=1)
# and still pass the post-release `braid ack` (lock is free). Mirrors
# `acquire_with_timeout_polls_then_succeeds_after_holder_release` in
# cli/src/pool_lock.rs at the integration seam.
# Scenario: a concurrent braid operation is holding the pool lock when
# the user runs `braid ack`. ack enters its bounded wait, and when the
# concurrent operation finishes ack should re-acquire promptly and
# ack normally -- not wait out the full 10 s timeout.
with subtest("ack re-acquires promptly when holder releases mid-wait"):
    write_missing_latch(1)
    write_acked_stats({"1": acked_disk(False, 23)})
    machine.succeed("systemctl start braid-alert.service")
    machine.succeed(
        "rm -f /tmp/ack.started /tmp/ack.done /tmp/ack.rc /tmp/ack.out"
    )

    holder_pid = start_lock_holder_until_release()
    try:
        # `touch /tmp/ack.started` lives in the wrapper immediately
        # before `braid ack` so we can synchronize on the wrapper
        # actually reaching the invocation point, rather than on the
        # shell having been backgrounded. Without this, a slow or
        # paused VM could let the test release the holder before
        # ack ever entered the lock path -- the test would then pass
        # without exercising mid-wait re-acquire.
        machine.succeed(
            "nohup sh -c "
            "'touch /tmp/ack.started; "
            "braid ack >/tmp/ack.out 2>&1; echo $? >/tmp/ack.rc; "
            "touch /tmp/ack.done' "
            ">/dev/null 2>&1 &"
        )
        machine.wait_until_succeeds(
            "test -e /tmp/ack.started", timeout=10
        )
        # Sentinel proves the wrapper reached `braid ack`. Give the
        # process a brief window to parse argv, clear the root gate,
        # and reach acquire_with_timeout, then prove it is blocked on
        # the held lock -- no timing assertion involved. Config load
        # for `Commands::Ack` happens *after* the lock is acquired
        # (cli/src/main.rs:489 takes the pool lock at dispatch before
        # the match arm runs), per ADR 026, so it is not part of the
        # pre-acquire startup gap.
        time.sleep(2)
        rc, _ = machine.execute("test -e /tmp/ack.done")
        assert rc != 0, "ack completed while pool lock was still held"

        # Release the holder and measure how long ack takes to finish.
        # Bounded: one poll interval (~250 ms) plus ack's own work.
        release_start = time.monotonic()
        machine.succeed("touch /tmp/holder.release")
        machine.wait_until_succeeds("test -e /tmp/ack.done", timeout=5)
        release_to_done = time.monotonic() - release_start
    finally:
        release_lock_holder(holder_pid)

    ack_rc = int(machine.succeed("cat /tmp/ack.rc").strip())
    ack_out = machine.succeed("cat /tmp/ack.out")

    assert ack_rc == 0, (
        f"expected ack success after holder release, got rc={ack_rc}; "
        f"out={ack_out}"
    )
    assert release_to_done <= 5, (
        f"ack did not re-acquire promptly after release; "
        f"release_to_done={release_to_done:.2f}s; out={ack_out}"
    )
    machine.fail(f"test -e {quote(alert_latch_path)}")
    machine.fail("systemctl is-active --quiet braid-alert.service")


# Intent: remove fails fast under a held pool lock before pruning
# acked-stats.json for the target devid.
# Why it exists: remove writes acked-stats.json on success; leaving it
# outside the pool lock breaks the alert-state-mutator invariant.
# Scenario: The user tries to remove disk3 while another pool operation
# is in progress.
with subtest("remove fails fast without pruning acked-stats"):
    disk1_devid = get_pool_devid("disk1")
    disk3_devid = get_pool_devid("disk3")
    write_acked_stats(
        {
            str(disk1_devid): acked_disk(False, 11),
            str(disk3_devid): acked_disk(True, 33),
        }
    )
    acked_before = read_file_or_absent(acked_stats_path)

    holder_pid = start_lock_holder()
    try:
        rc, out = run_with_timeout("braid remove disk3 --yes", 2)
    finally:
        stop_lock_holder(holder_pid)

    assert rc != 0, f"expected remove contention failure, got rc=0; out={out}"
    assert rc != 124, f"remove hung past timeout; out={out}"
    assert_contention_message(out)
    assert_file_unchanged(acked_stats_path, acked_before)
    fi_show = machine.succeed("btrfs filesystem show /mnt/storage")
    assert "/dev/mapper/braid-disk3" in fi_show, (
        f"disk3 should still be a pool member:\n{fi_show}"
    )


with subtest("Setup: make disk3 missing"):
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk3")
    machine.succeed("mount -o degraded /dev/mapper/braid-disk1 /mnt/storage")
    fi_show = machine.succeed("btrfs filesystem show /mnt/storage")
    assert "missing" in fi_show.lower(), (
        f"expected degraded pool with missing disk:\n{fi_show}"
    )


# Intent: remove-missing fails fast under a held pool lock before pruning
# acked-stats.json for the missing devid.
# Why it exists: remove-missing writes acked-stats.json on success; it
# must be serialized with monitor, ack, add, and remove.
# Scenario: The user tries to clean up a missing disk while another braid
# operation is in progress.
with subtest("remove-missing fails fast without pruning acked-stats"):
    disk1_devid = get_pool_devid("disk1")
    missing_devid = get_missing_devid()
    write_acked_stats(
        {
            str(disk1_devid): acked_disk(False, 11),
            str(missing_devid): acked_disk(True, 44),
        }
    )
    acked_before = read_file_or_absent(acked_stats_path)

    holder_pid = start_lock_holder()
    try:
        rc, out = run_with_timeout(
            f"braid remove-missing --missing-id {missing_devid} --yes",
            2,
        )
    finally:
        stop_lock_holder(holder_pid)

    assert rc != 0, (
        f"expected remove-missing contention failure, got rc=0; out={out}"
    )
    assert rc != 124, f"remove-missing hung past timeout; out={out}"
    assert_contention_message(out)
    assert_file_unchanged(acked_stats_path, acked_before)
    assert get_missing_devid() == missing_devid, (
        "missing devid disappeared despite held pool lock"
    )


# Intent: live-pool add fails fast under a held pool lock before dropping
# the acked-stats ghost for the next assigned devid.
# Why it exists: add also mutates acked-stats.json; lock coverage must
# include the live-pool path, not just bootstrap add.
# Scenario: A degraded pool is mounted and the user starts adding disk4
# while another braid operation owns the pool lock.
with subtest("add fails fast without pruning next-devid acked-stats"):
    missing_devid = get_missing_devid()
    next_devid = max(btrfs_show_devids() + [missing_devid]) + 1
    write_acked_stats(
        {
            str(next_devid): acked_disk(True, 55),
            str(missing_devid): acked_disk(True, 44),
        }
    )
    acked_before = read_file_or_absent(acked_stats_path)

    holder_pid = start_lock_holder()
    try:
        rc, out = run_with_timeout(add_cmd("disk4"), 2)
    finally:
        stop_lock_holder(holder_pid)

    assert rc != 0, f"expected add contention failure, got rc=0; out={out}"
    assert rc != 124, f"add hung past timeout; out={out}"
    assert_contention_message(out)
    assert_file_unchanged(acked_stats_path, acked_before)
    fi_show = machine.succeed("btrfs filesystem show /mnt/storage")
    assert "/dev/mapper/braid-disk4" not in fi_show, (
        f"disk4 should not have joined the pool:\n{fi_show}"
    )
    machine.fail("test -e /dev/mapper/braid-disk4")
    machine.fail("cryptsetup isLuks /dev/disk/by-id/virtio-disk4")


machine.shutdown()
