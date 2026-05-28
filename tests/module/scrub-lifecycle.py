# Test: scrub-lifecycle
#
# Intent: Verify the key behaviors of lifecycle-bound scrub: (1) Persistent
#   catch-up fires immediately after pool unlock when the timer stamp is overdue,
#   (2) braid lock succeeds while the scrub service is actively holding the
#   mount busy, because Rust dispatch stops the timer and service first, (3) the
#   pool-online trigger resumes a previously cancelled scrub, and (4) an
#   overdue timer fire and a resumable pool-online state both target
#   braid-scrub.service. systemd coalesces overlapping start jobs into one run;
#   the run resumes the saved scrub and satisfies the overdue timer fire without
#   producing a second scrub or a "Scrub is already running" error.
#
# Why it exists: Config tests verify unit properties (BindsTo, Persistent, etc.)
#   but only a behavioral test proves the catch-up actually fires, the
#   cancellation path works end-to-end, the resume trigger engages, and the
#   single-runner topology prevents duplicate btrfs scrub processes.
#
# Scenario: Four nodes, each with a 2-disk RAID1 pool.
#   catchup:     real scrub service, seeded overdue stamp, unlock triggers catch-up.
#   cancel:      fake long-running scrub (holds mount busy), lock succeeds because
#                Rust dispatch stops timer+service before CLI unmounts.
#   resume:      real scrub on dm-delay-backed disks, cancel mid-scrub, then
#                resume via the pool-online trigger on next unlock.
#   concurrency: dm-delay-backed pool with saved scrub progress + overdue timer
#                stamp; on unlock, both activation paths target braid-scrub.service.

import json
import shlex

start_all()

passphrase = "testpassphrase"
pq = shlex.quote(passphrase)

TIMER = "braid-scrub.timer"
SERVICE = "braid-scrub.service"
TRIGGER_SERVICE = "braid-scrub-resume-trigger.service"
STAMP = "/var/lib/systemd/timers/stamp-braid-scrub.timer"
# TODO: Play with these values to speed up test. This test is really slow.
SCRUB_PAYLOAD_MIB = 32
SCRUB_READ_DELAY_MS = 500
SCRUB_DELAY_DISKS = ["disk1", "disk2"]


def show(node, unit, prop):
    return node.succeed(
        "systemctl show {} -p {} --value".format(unit, prop)
    ).strip()


def luks_uuid_for_name(name):
    return {
        "disk1": "11111111-1111-1111-1111-111111111111",
        "disk2": "22222222-2222-2222-2222-222222222222",
    }[name]


def setup_resume_pool(node):
    for name in SCRUB_DELAY_DISKS:
        dm_delay_create(node, name)
        by_id = "/dev/disk/by-id/braid-test-{}-delay".format(name)
        node.succeed(
            "printf '%s' {} | cryptsetup luksFormat --batch-mode --key-file=- "
            "--pbkdf pbkdf2 --pbkdf-force-iterations 1000 "
            "--uuid {} --label braid-{} {}".format(
                pq, luks_uuid_for_name(name), name, by_id
            )
        )
        node.succeed(
            "printf '%s' {} | cryptsetup open --key-file=- {} braid-{}".format(
                pq, by_id, name
            )
        )

    node.succeed(
        "mkfs.btrfs -f -d raid1 -m raid1 "
        "/dev/mapper/braid-disk1 /dev/mapper/braid-disk2"
    )
    node.succeed("cryptsetup close braid-disk1")
    node.succeed("cryptsetup close braid-disk2")
    pool_json = {
        "disks": {
            luks_uuid_for_name(name): {
                "name": name,
                "by_id": "/dev/disk/by-id/braid-test-{}-delay".format(name),
            }
            for name in SCRUB_DELAY_DISKS
        }
    }
    node.succeed(
        "cat > /var/lib/braid/pool.json << 'EOF'\n{}\nEOF".format(
            json.dumps(pool_json)
        )
    )


def unlock(node):
    node.succeed("printf '%s\\n' {} | braid unlock --passphrase-stdin".format(pq))


def wait_unit_success_after(node, unit, old_ts, timeout=120):
    node.wait_until_succeeds(
        "test \"$(systemctl show {} -p ExecMainStartTimestampMonotonic --value)\" != '{}'"
        " && test \"$(systemctl show {} -p ExecMainStartTimestampMonotonic --value)\" != '0'"
        " && test \"$(systemctl show {} -p Result --value)\" = success"
        " && ! systemctl is-active --quiet {}".format(unit, old_ts, unit, unit, unit),
        timeout=timeout,
    )


def wait_online_stop_settled(node):
    node.wait_until_succeeds(
        "! systemctl is-active --quiet braid-online.service "
        "&& ! systemctl is-active --quiet {}".format(SERVICE),
        timeout=30,
    )


def disable_trigger_hook(node):
    node.succeed("mkdir -p /run/systemd/system/{}.d".format(TRIGGER_SERVICE))
    node.succeed(
        "printf '%s\\n' '[Unit]' 'ConditionPathExists=/run/braid-enable-scrub-resume' "
        "> /run/systemd/system/{}.d/skip.conf".format(TRIGGER_SERVICE)
    )
    node.succeed("systemctl daemon-reload")


def enable_trigger_hook(node):
    node.succeed("rm -f /run/systemd/system/{}.d/skip.conf".format(TRIGGER_SERVICE))
    node.succeed("systemctl daemon-reload")


# === catchup node: Persistent catch-up ===

with subtest("catchup: timer inactive before unlock"):
    catchup.wait_for_unit("multi-user.target", timeout=120)
    catchup.succeed("systemctl cat {}".format(TIMER))
    catchup.fail("systemctl is-active {}".format(TIMER))

with subtest("catchup: Persistent catch-up fires on unlock with overdue stamp"):
    # Seed old stamp file to create explicit overdue state. This simulates a
    # timer that last fired on 2025-01-01 — well past the monthly boundary.
    catchup.succeed("mkdir -p /var/lib/systemd/timers")
    catchup.succeed("touch -t 202501010000 {}".format(STAMP))

    catchup.succeed(
        "printf '%s\\n' {} | braid unlock --passphrase-stdin".format(pq)
    )
    catchup.succeed("systemctl is-active braid-online.service")
    catchup.succeed("systemctl is-active {}".format(TIMER))

    # Persistent=true reads old stamp, fires immediately. Scrub on tiny disk
    # completes in milliseconds, so check Result (not ActiveState).
    catchup.wait_until_succeeds(
        "test \"$(systemctl show {} -p Result --value)\" = success".format(
            SERVICE
        ),
        timeout=30,
    )

with subtest("catchup: timer stops when pool is locked"):
    catchup.succeed("braid lock")
    catchup.fail("systemctl is-active {}".format(TIMER))
    catchup.fail("systemctl is-active braid-online.service")

with subtest("catchup: catch-up fires again after stamp is re-aged"):
    # Record monotonic timestamp from the previous scrub run.
    old_ts = show(catchup, SERVICE, "ExecMainStartTimestampMonotonic")

    # Age the stamp back to 2025-01-01. The timer is stopped (lock stopped
    # braid-online → BindsTo stopped the timer), so systemd won't interfere.
    catchup.succeed("touch -t 202501010000 {}".format(STAMP))

    catchup.succeed(
        "printf '%s\\n' {} | braid unlock --passphrase-stdin".format(pq)
    )

    # Wait for a NEW scrub activation (timestamp must differ from old run).
    catchup.wait_until_succeeds(
        "test \"$(systemctl show {} -p ExecMainStartTimestampMonotonic --value)\" != '{}'"
        " && test \"$(systemctl show {} -p Result --value)\" = success".format(
            SERVICE, old_ts, SERVICE
        ),
        timeout=30,
    )

catchup.shutdown()

# === cancel node: safe cancellation during lock ===

with subtest("cancel: unlock and wait for fake scrub active"):
    cancel.wait_for_unit("multi-user.target", timeout=120)

    # Seed old stamp so Persistent fires the fake scrub immediately on unlock.
    cancel.succeed("mkdir -p /var/lib/systemd/timers")
    cancel.succeed("touch -t 202501010000 {}".format(STAMP))

    cancel.succeed(
        "printf '%s\\n' {} | braid unlock --passphrase-stdin".format(pq)
    )

    # Wait for fake scrub to be actively running (sleep 300 with open FD).
    cancel.wait_until_succeeds(
        "systemctl is-active {}".format(SERVICE)
    )

with subtest("cancel: btrfs upstream contract -- `btrfs scrub cancel` on no-scrub mount exits 2 (ENOTCONN)"):
    # Pin btrfs-progs's source-pinned ENOTCONN exit code, which braid's
    # typed scrub_cancel dispatch (cli/src/scrub_cancel.rs) depends on.
    # The cancel-idle exit code is not documented in btrfs-scrub(8); it is
    # an implementation contract in the pinned btrfs-progs source. The fake
    # scrub service is up but never issued `btrfs scrub start`, so the kernel
    # has no scrub -- the ioctl returns ENOTCONN deterministically. If a
    # future nixpkgs bump ever changes this exit code, this subtest fails
    # before braid's typed dispatch can silently misclassify.
    # Source: reference/btrfs-progs/cmds/scrub.c:1794-1812.
    rc, out = cancel.execute("btrfs scrub cancel /mnt/storage")
    assert rc == 2, (
        "expected btrfs scrub cancel to exit 2 (ENOTCONN) on a no-scrub "
        f"mount, got rc={rc} (output: {out!r})"
    )

with subtest("cancel: lock succeeds while scrub holds mount busy"):
    # Lock while scrub service is actively holding the mount.
    # Rust dispatch must stop the timer and service before it attempts unmount.
    cancel.succeed("braid lock")
    cancel.fail("mountpoint -q /mnt/storage")
    cancel.fail("test -e /dev/mapper/braid-disk1")
    cancel.fail("test -e /dev/mapper/braid-disk2")

with subtest("cancel: ExecStop succeeded (idle cancel is benign)"):
    # The fake scrub never ran `btrfs scrub start`, so raw `btrfs scrub cancel`
    # returns ENOTCONN / exit 2. `braid scrub-cancel` maps that idle-cancel
    # result to success, so ExecStop must not mark the unit failed. This
    # subtest is the failure-layer guard for the old false-fail bug.
    result = show(cancel, SERVICE, "Result")
    assert result == "success", f"braid-scrub.service ExecStop failed: Result={result}"

cancel.shutdown()

# === resume node: cancel and resume real btrfs scrub ===

with subtest("resume: prepare dm-delay backed pool"):
    resume.wait_for_unit("multi-user.target", timeout=120)
    setup_resume_pool(resume)
    unlock(resume)
    resume.succeed(
        "dd if=/dev/urandom of=/mnt/storage/scrub-payload bs=1M count={} status=none".format(
            SCRUB_PAYLOAD_MIB
        )
    )
    resume.succeed("sync")
    dm_delay_activate(resume, SCRUB_DELAY_DISKS, read_delay_ms=SCRUB_READ_DELAY_MS)

with subtest("resume: cancel preserves Aborted state across lock/unlock"):
    disable_trigger_hook(resume)
    resume.succeed("systemctl start {}".format(SERVICE))
    resume.succeed(
        "for i in $(seq 1 400); do "
        "out=\"$(btrfs scrub status --raw /mnt/storage 2>&1 || true)\"; "
        "if printf '%s\\n' \"$out\" | grep -Eq 'Status:[[:space:]]+running'; "
        "then exit 0; fi; sleep 0.05; done; "
        "printf '%s\\n' \"$out\"; exit 1"
    )

    resume.succeed("braid lock")
    dm_delay_deactivate(resume, SCRUB_DELAY_DISKS)
    unlock(resume)
    resume.wait_until_succeeds(
        "btrfs scrub status --raw /mnt/storage | grep -Eq 'Status:[[:space:]]+aborted'",
        timeout=30,
    )

with subtest("resume: pool-online hook resumes a cancelled scrub"):
    enable_trigger_hook(resume)
    old_trigger_ts = show(resume, TRIGGER_SERVICE, "ExecMainStartTimestampMonotonic")
    old_scrub_ts = show(resume, SERVICE, "ExecMainStartTimestampMonotonic")
    resume.succeed("systemctl start {}".format(TRIGGER_SERVICE))
    wait_unit_success_after(resume, TRIGGER_SERVICE, old_trigger_ts, timeout=30)
    wait_unit_success_after(resume, SERVICE, old_scrub_ts, timeout=180)
    status = resume.succeed("btrfs scrub status --raw /mnt/storage")
    assert "Scrub resumed:" in status, status

with subtest("resume: pool-online hook no-ops when nothing to resume"):
    old_trigger_ts = show(resume, TRIGGER_SERVICE, "ExecMainStartTimestampMonotonic")
    old_scrub_ts = show(resume, SERVICE, "ExecMainStartTimestampMonotonic")
    resume.succeed("systemctl start {}".format(TRIGGER_SERVICE))
    wait_unit_success_after(resume, TRIGGER_SERVICE, old_trigger_ts, timeout=30)
    new_scrub_ts = show(resume, SERVICE, "ExecMainStartTimestampMonotonic")
    assert new_scrub_ts == old_scrub_ts, (
        "no resumable state should not start scrub service; old={}, new={}".format(
            old_scrub_ts, new_scrub_ts
        )
    )
    resume.fail("systemctl is-active {}".format(SERVICE))

with subtest("resume: lock/unlock with fresh timer stamp does not start a new scrub"):
    enable_trigger_hook(resume)
    resume.succeed("mkdir -p /var/lib/systemd/timers")
    resume.succeed("touch {}".format(STAMP))

    resume.succeed("braid lock")
    wait_online_stop_settled(resume)
    old_trigger_ts = show(resume, TRIGGER_SERVICE, "ExecMainStartTimestampMonotonic")
    old_scrub_ts = show(resume, SERVICE, "ExecMainStartTimestampMonotonic")
    unlock(resume)
    wait_unit_success_after(resume, TRIGGER_SERVICE, old_trigger_ts, timeout=30)
    new_scrub_ts = show(resume, SERVICE, "ExecMainStartTimestampMonotonic")
    assert new_scrub_ts in [old_scrub_ts, "0"], (
        "fresh timer stamp should not start a new scrub; old={}, new={}".format(
            old_scrub_ts, new_scrub_ts
        )
    )
    resume.fail("systemctl is-active {}".format(SERVICE))

with subtest("resume: lock/unlock with aged timer stamp fires scheduled scrub"):
    dm_delay_deactivate(resume, SCRUB_DELAY_DISKS)
    resume.succeed("braid lock")
    wait_online_stop_settled(resume)
    resume.succeed("touch -t 202501010000 {}".format(STAMP))
    old_trigger_ts = show(resume, TRIGGER_SERVICE, "ExecMainStartTimestampMonotonic")
    old_scrub_ts = show(resume, SERVICE, "ExecMainStartTimestampMonotonic")
    unlock(resume)
    wait_unit_success_after(resume, TRIGGER_SERVICE, old_trigger_ts, timeout=30)
    wait_unit_success_after(resume, SERVICE, old_scrub_ts, timeout=120)

resume.shutdown()

# === concurrency node: timer and trigger coalesce on one scrub service ===

# Intent: an overdue timer fire and a resumable pool-online state both target
#   braid-scrub.service. systemd coalesces overlapping start jobs into one run;
#   the run resumes the saved scrub and satisfies the overdue timer fire without
#   producing a second scrub or a "Scrub is already running" error.
# Why it exists: before the single-runner topology, two foreground btrfs scrub
#   units could race unless an external flock serialized them.
# Scenario: user unlocks a pool that has saved scrub progress AND is overdue
#   for its scheduled monthly scrub. One shared scrub service run should resume
#   progress, finish cleanly, and absorb both activation paths.

with subtest("concurrency: prepare dm-delay backed pool with saved scrub progress"):
    concurrency.wait_for_unit("multi-user.target", timeout=120)
    setup_resume_pool(concurrency)
    unlock(concurrency)
    concurrency.succeed(
        "dd if=/dev/urandom of=/mnt/storage/scrub-payload bs=1M count={} status=none".format(
            SCRUB_PAYLOAD_MIB
        )
    )
    concurrency.succeed("sync")

    # Slow I/O so the in-flight scrub stays running long enough for the
    # explicit btrfs scrub cancel below to land before it finishes.
    dm_delay_activate(
        concurrency,
        SCRUB_DELAY_DISKS,
        read_delay_ms=SCRUB_READ_DELAY_MS,
    )
    # Mask the resume trigger before starting the scrub. daemon-reload can take
    # several seconds in the VM; doing it after the scrub starts can let the
    # scrub finish before the cancel lands, leaving no resumable state.
    disable_trigger_hook(concurrency)
    concurrency.succeed("systemctl start {}".format(SERVICE))
    concurrency.succeed(
        "for i in $(seq 1 400); do "
        "out=\"$(btrfs scrub status --raw /mnt/storage 2>&1 || true)\"; "
        "if printf '%s\\n' \"$out\" | grep -Eq 'Status:[[:space:]]+running'; "
        "then exit 0; fi; sleep 0.05; done; "
        "printf '%s\\n' \"$out\"; exit 1"
    )

    # Cancel directly via btrfs and assert the resumable precondition,
    # instead of racing a full braid lock pipeline against scrub completion.
    # The lock-cancels-scrub path is covered by the resume node's
    # "cancel preserves Aborted state across lock/unlock" subtest; this
    # subtest only needs a saved scrub state as a precondition so the
    # "Scrub resumed:" assertion below proves coalesced timer+trigger
    # activation, not a working cancel. btrfs reports the saved state as
    # "aborted" (canceled=1) or "interrupted" (canceled=0/finished=0); both
    # are resumable (reference/btrfs-progs/cmds/scrub.c:1430).
    concurrency.succeed("btrfs scrub cancel /mnt/storage")
    concurrency.wait_until_succeeds(
        "btrfs scrub status --raw /mnt/storage | "
        "grep -Eq 'Status:[[:space:]]+(aborted|interrupted)'",
        timeout=30,
    )

    concurrency.succeed("braid lock")
    wait_online_stop_settled(concurrency)
    # Reset dm-delay so the offline setup work is fast; we re-arm the delay
    # before the actual race trigger below.
    dm_delay_deactivate(concurrency, SCRUB_DELAY_DISKS)

with subtest("concurrency: overdue timer + resumable state coalesce into one scrub"):
    # Age the timer stamp so Persistent=true fires immediately on next unlock.
    concurrency.succeed("mkdir -p /var/lib/systemd/timers")
    concurrency.succeed("touch -t 202501010000 {}".format(STAMP))
    enable_trigger_hook(concurrency)

    # Re-arm dm-delay so any accidental second scrub has time to become visible
    # through a changed ExecMainStartTimestampMonotonic.
    dm_delay_activate(
        concurrency,
        SCRUB_DELAY_DISKS,
        read_delay_ms=SCRUB_READ_DELAY_MS,
    )

    old_trigger_ts = show(concurrency, TRIGGER_SERVICE, "ExecMainStartTimestampMonotonic")
    old_scrub_ts = show(concurrency, SERVICE, "ExecMainStartTimestampMonotonic")

    # Trigger the race: braid-online activates the resume trigger via WantedBy
    # AND starts braid-scrub.timer; the timer fires immediately because
    # Persistent=true sees the aged stamp.
    unlock(concurrency)

    wait_unit_success_after(concurrency, TRIGGER_SERVICE, old_trigger_ts, timeout=30)
    wait_unit_success_after(concurrency, SERVICE, old_scrub_ts, timeout=300)
    dm_delay_deactivate(concurrency, SCRUB_DELAY_DISKS)

    completed_scrub_ts = show(concurrency, SERVICE, "ExecMainStartTimestampMonotonic")
    concurrency.succeed("sleep 5")
    later_scrub_ts = show(concurrency, SERVICE, "ExecMainStartTimestampMonotonic")
    assert later_scrub_ts == completed_scrub_ts, (
        "timer and trigger should coalesce into one scrub run; completed={}, later={}".format(
            completed_scrub_ts, later_scrub_ts
        )
    )
    status = concurrency.succeed("btrfs scrub status --raw /mnt/storage")
    assert "Scrub resumed:" in status, status

with subtest("concurrency: no 'Scrub is already running' in the journal"):
    # The race-loser's btrfs invocation would emit "Scrub is already running"
    # (reference/btrfs-progs/cmds/scrub.c:1392-1398) on contention. The single
    # scrub service topology prevents that; assert it never appeared.
    concurrency.fail(
        "journalctl -u {} -u {} --no-pager | grep -F 'Scrub is already running'".format(
            SERVICE, TRIGGER_SERVICE
        )
    )

concurrency.shutdown()
