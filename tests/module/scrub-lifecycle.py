# Test: scrub-lifecycle
#
# Intent: Verify the behaviors freshness-driven scrub scheduling rests on:
#   (1) the timer's post-unlock poke scrubs a pool btrfs has never scrubbed,
#   (2) the very next unlock does NOT re-scrub it, because btrfs's own record
#       now says the pool is fresh -- the flagship consequence of ADR 035,
#   (3) shrinking the freshness window makes that same record stale, and the
#       next poke scrubs again,
#   (4) `braid lock` succeeds while the scrub service is actively holding the
#       mount busy, because Rust dispatch stops the timer and service first,
#   (5) the post-unlock poke resumes a previously cancelled scrub, with no
#       resume-trigger unit involved, and
#   (6) a poke that lands while a scrub is already running starts no second
#       scrub and raises no alert.
#
# Why it exists: config tests verify unit properties (BindsTo, OnActiveSec) but
#   only a behavioral test proves btrfs's record actually drives the decision.
#   The suppression case is the one that used to be impossible: under the old
#   calendar timer a scrub finished on the 30th was followed by another on the
#   1st. The concurrency case guards the other direction -- an hourly poll now
#   lands on running scrubs routinely, and each one must be a quiet exit 0
#   rather than the "Scrub is already running" failure it used to be.
#
# Scenario: Four nodes, each with a 2-disk RAID1 pool.
#   freshness:   unlock scrubs a never-scrubbed pool; the next unlock does not;
#                a one-second window makes the record stale and it scrubs again.
#   cancel:      fake long-running scrub (holds mount busy), lock succeeds because
#                Rust dispatch stops timer+service before CLI unmounts.
#   resume:      real scrub on dm-delay-backed disks, cancel mid-scrub, then
#                resume via the post-unlock poke on next unlock.
#   concurrency: dm-delay-backed pool with a real scrub in flight when a poke
#                arrives.

import json
import shlex

start_all()

passphrase = "testpassphrase"
pq = shlex.quote(passphrase)

TIMER = "braid-scrub.timer"
SERVICE = "braid-scrub.service"
MOUNT = "/mnt/storage"
SCRUB_CANCEL_REQUESTED_FLAG = "/var/lib/braid/scrub-cancel-requested"
# TODO: Play with these values to speed up test. This test is really slow.
SCRUB_PAYLOAD_MIB = 32
SCRUB_READ_DELAY_MS = 500
SCRUB_DELAY_DISKS = ["disk1", "disk2"]


def show(node, unit, prop):
    return node.succeed(
        "systemctl show {} -p {} --value".format(unit, prop)
    ).strip()


def set_fresh_for(node, secs):
    """Shrink (or restore) the unit's freshness window.

    The only sanctioned way to age a scrub record in these tests: moving the
    guest clock would desynchronize it from the naive-local ctime btrfs writes,
    and hand-editing /var/lib/btrfs/scrub.status.<fsid> would test a file braid
    never writes. Overriding the window instead leaves btrfs's record exactly as
    btrfs left it and changes only the question braid asks about it.
    """
    node.succeed("mkdir -p /run/systemd/system/{}.d".format(SERVICE))
    node.succeed(
        "printf '%s\\n' '[Service]' 'ExecStart=' "
        "'ExecStart=/run/current-system/sw/bin/braid scrub-resume-or-start "
        "--mount {} --fresh-for-secs {}' "
        "> /run/systemd/system/{}.d/window.conf".format(MOUNT, secs, SERVICE)
    )
    node.succeed("systemctl daemon-reload")


def scrub_anchor(node):
    """btrfs's own scheduling anchor: the latest start-or-resume it recorded."""
    return node.succeed(
        "btrfs scrub status --raw {} | grep -E 'Scrub (started|resumed):'".format(
            MOUNT
        )
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


# === freshness node: btrfs's record decides whether a scrub runs ===

with subtest("freshness: timer inactive before unlock"):
    freshness.wait_for_unit("multi-user.target", timeout=120)
    freshness.succeed("systemctl cat {}".format(TIMER))
    freshness.fail("systemctl is-active {}".format(TIMER))

with subtest("freshness: the post-unlock poke scrubs a never-scrubbed pool"):
    # No stamp file, no calendar boundary: OnActiveSec=30s pokes the service
    # shortly after the timer comes up with braid-online, and btrfs's "no stats
    # available" is a due pool.
    before_first = show(freshness, SERVICE, "ExecMainStartTimestampMonotonic")
    unlock(freshness)
    freshness.succeed("systemctl is-active braid-online.service")
    freshness.succeed("systemctl is-active {}".format(TIMER))
    # The precondition, checked inside the 30s poke window: nothing has ever
    # scrubbed this pool, so the scrub below is the poke's doing and not a
    # leftover.
    freshness.succeed(
        "btrfs scrub status --raw {} | grep -q 'no stats available'".format(MOUNT)
    )
    # Wait for the service run to *finish*, not merely for btrfs to write
    # `finished`: the runner is still holding the pool lock across its
    # post-spawn confirmation at that moment, and the `braid lock` below would
    # be refused as contended.
    wait_unit_success_after(freshness, SERVICE, before_first, timeout=180)
    freshness.succeed(
        "btrfs scrub status --raw {} | grep -Eq 'Status:[[:space:]]+finished'".format(
            MOUNT
        )
    )
    first_anchor = scrub_anchor(freshness)

with subtest("freshness: the next unlock does not re-scrub a fresh pool"):
    # The flagship case. Under the old calendar timer this unlock would have
    # fired a Persistent catch-up regardless of the scrub that just finished.
    freshness.succeed("braid lock")
    freshness.fail("systemctl is-active {}".format(TIMER))
    before_poke = show(freshness, SERVICE, "ExecMainStartTimestampMonotonic")

    unlock(freshness)
    # The poke still *runs* -- it just decides nothing is owed. Waiting for the
    # run rather than for a sleep keeps the assertion below from passing merely
    # because the poke had not fired yet.
    wait_unit_success_after(freshness, SERVICE, before_poke, timeout=180)
    freshness.succeed("journalctl -u {} | grep -q 'scrub not due'".format(SERVICE))
    freshness.succeed(
        "journalctl -u {} | grep -q 'last scrub started/resumed'".format(SERVICE)
    )
    assert scrub_anchor(freshness) == first_anchor, (
        "a fresh pool must not be re-scrubbed; anchor moved from {} to {}".format(
            first_anchor, scrub_anchor(freshness)
        )
    )

with subtest("freshness: a stale record scrubs again on the next poke"):
    # Same btrfs record, smaller window: what changed is only the question
    # braid asks about it.
    freshness.succeed("braid lock")
    wait_online_stop_settled(freshness)
    set_fresh_for(freshness, 1)
    before_stale = show(freshness, SERVICE, "ExecMainStartTimestampMonotonic")

    unlock(freshness)
    wait_unit_success_after(freshness, SERVICE, before_stale, timeout=180)
    freshness.succeed(
        "btrfs scrub status --raw {} | grep -Eq 'Status:[[:space:]]+finished'".format(
            MOUNT
        )
    )
    assert scrub_anchor(freshness) != first_anchor, (
        "a stale record must produce a new scrub; anchor still {}".format(
            first_anchor
        )
    )

freshness.shutdown()

# === cancel node: safe cancellation during lock ===

with subtest("cancel: unlock and wait for fake scrub active"):
    cancel.wait_for_unit("multi-user.target", timeout=120)
    cancel.succeed(
        "printf '%s\\n' {} | braid unlock --passphrase-stdin".format(pq)
    )
    # The fake ExecStart replaces braid entirely, so start it explicitly rather
    # than waiting on the poke: this node is about the stop path, not scheduling.
    cancel.succeed("systemctl start --no-block {}".format(SERVICE))
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
    cancel.succeed("test -f {}".format(SCRUB_CANCEL_REQUESTED_FLAG))
    mode = cancel.succeed("stat -c %a {}".format(SCRUB_CANCEL_REQUESTED_FLAG)).strip()
    assert mode == "600", (
        "expected scrub-cancel-requested mode 600, got {}".format(mode)
    )

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
    # Mask the timer for the setup: every scrub run below must come from a poke
    # or start this test issues, not from an incidental post-unlock poke.
    resume.succeed("systemctl mask --runtime {}".format(TIMER))
    unlock(resume)
    resume.succeed(
        "dd if=/dev/urandom of=/mnt/storage/scrub-payload bs=1M count={} status=none".format(
            SCRUB_PAYLOAD_MIB
        )
    )
    resume.succeed("sync")
    dm_delay_activate(resume, SCRUB_DELAY_DISKS, read_delay_ms=SCRUB_READ_DELAY_MS)

with subtest("resume: cancel preserves Aborted state across lock/unlock"):
    resume.succeed("systemctl start --no-block {}".format(SERVICE))
    resume.succeed(
        "for i in $(seq 1 400); do "
        "out=\"$(btrfs scrub status --raw /mnt/storage 2>&1 || true)\"; "
        "if printf '%s\\n' \"$out\" | grep -Eq 'Status:[[:space:]]+running'; "
        "then exit 0; fi; sleep 0.05; done; "
        "printf '%s\\n' \"$out\"; exit 1"
    )

    resume.succeed("braid lock")
    # A real lock-time cancel makes btrfs exit 1, but the cancel-request marker
    # ExecStop wrote lets scrub-resume-or-start exit 0, so the unit is success,
    # not failed. This is exactly what keeps onFailure (-> ScrubFailed alert)
    # from firing on every lock/suspend/shutdown; the dedicated end-to-end proof
    # is the scrub-alert test, but pin Result here too since this subtest is the
    # one that already cancels a real mid-flight scrub.
    cancel_result = show(resume, SERVICE, "Result")
    assert cancel_result == "success", (
        "lock-cancelled real scrub must be success, got Result={}".format(cancel_result)
    )
    dm_delay_deactivate(resume, SCRUB_DELAY_DISKS)
    unlock(resume)
    resume.wait_until_succeeds(
        "btrfs scrub status --raw /mnt/storage | grep -Eq 'Status:[[:space:]]+aborted'",
        timeout=30,
    )

with subtest("resume: the post-unlock poke resumes a cancelled scrub"):
    # No resume-trigger unit exists any more: OnActiveSec=30s is what makes
    # "an aborted scrub is resumed shortly after unlock" true, because an
    # aborted record classifies as due no matter how recent it is.
    resume.succeed("braid lock")
    wait_online_stop_settled(resume)
    resume.succeed("systemctl unmask --runtime {}".format(TIMER))
    old_scrub_ts = show(resume, SERVICE, "ExecMainStartTimestampMonotonic")
    unlock(resume)
    wait_unit_success_after(resume, SERVICE, old_scrub_ts, timeout=180)
    status = resume.succeed("btrfs scrub status --raw /mnt/storage")
    assert "Scrub resumed:" in status, status

with subtest("resume: the next poke leaves the freshly resumed scrub alone"):
    # The resumed scrub finished moments ago, so its resume timestamp is the
    # anchor and the pool is fresh. A poke now must be a no-op -- this is the
    # half that keeps a resume from turning into a scrub loop.
    anchor = scrub_anchor(resume)
    old_scrub_ts = show(resume, SERVICE, "ExecMainStartTimestampMonotonic")
    resume.succeed("systemctl start {}".format(SERVICE))
    wait_unit_success_after(resume, SERVICE, old_scrub_ts, timeout=60)
    resume.succeed("journalctl -u {} | grep -q 'scrub not due'".format(SERVICE))
    assert scrub_anchor(resume) == anchor, (
        "a freshly resumed scrub must not be re-run; anchor moved from {}".format(
            anchor
        )
    )

resume.shutdown()

# === concurrency node: a poke during a running scrub ===

# Intent: an hourly poll that lands while a scrub is already running must exit 0
#   without starting a second scrub and without raising an alert.
# Why it exists: polling hourly makes this collision routine rather than rare.
#   braid used to let it reach `btrfs scrub resume`, which refuses with exit 1
#   ("Scrub is already running") -- and that exit 1 beeped the operator awake
#   for a pool that was being scrubbed correctly at that very moment.
# Scenario: a real scrub is in flight on slow (dm-delay) disks when the service
#   is started again.

with subtest("concurrency: start a real scrub on slow disks"):
    concurrency.wait_for_unit("multi-user.target", timeout=120)
    setup_resume_pool(concurrency)
    concurrency.succeed("systemctl mask --runtime {}".format(TIMER))
    unlock(concurrency)
    concurrency.succeed(
        "dd if=/dev/urandom of=/mnt/storage/scrub-payload bs=1M count={} status=none".format(
            SCRUB_PAYLOAD_MIB
        )
    )
    concurrency.succeed("sync")
    dm_delay_activate(
        concurrency,
        SCRUB_DELAY_DISKS,
        read_delay_ms=SCRUB_READ_DELAY_MS,
    )
    concurrency.succeed("systemctl start --no-block {}".format(SERVICE))
    concurrency.succeed(
        "for i in $(seq 1 400); do "
        "out=\"$(btrfs scrub status --raw /mnt/storage 2>&1 || true)\"; "
        "if printf '%s\\n' \"$out\" | grep -Eq 'Status:[[:space:]]+running'; "
        "then exit 0; fi; sleep 0.05; done; "
        "printf '%s\\n' \"$out\"; exit 1"
    )

with subtest("concurrency: a poke during the scrub starts no second scrub"):
    # Re-check "still running" immediately before the poke rather than trusting
    # the earlier check: this is the precondition that keeps the test from
    # passing vacuously against a scrub that already finished.
    concurrency.succeed(
        "btrfs scrub status --raw /mnt/storage | grep -Eq 'Status:[[:space:]]+running'"
    )
    running_anchor = scrub_anchor(concurrency)
    # The scrub service is already active, so a poke is a start job on a running
    # unit -- systemd coalesces it.
    concurrency.succeed("systemctl start --no-block {}".format(TIMER))
    concurrency.succeed("systemctl start --no-block {}".format(SERVICE))

    # The load-bearing assertion is btrfs's record, not a sampled `running`
    # state: a second scrub would restart the anchor, and that is observable
    # whether or not the first scrub happens to finish while we look. Sampling
    # `Status: running` after the poke would be a race against a scrub that is
    # allowed to complete at any moment.
    assert scrub_anchor(concurrency) == running_anchor, (
        "a poke must not restart the running scrub; anchor moved from {}".format(
            running_anchor
        )
    )

with subtest("concurrency: the poke raised no alert and no second scrub"):
    # The race-loser's btrfs invocation would emit "Scrub is already running"
    # (reference/btrfs-progs/cmds/scrub.c) on contention. Neither the entry
    # classifier nor the collision path may let that reach a failed unit.
    dm_delay_deactivate(concurrency, SCRUB_DELAY_DISKS)
    concurrency.wait_until_succeeds(
        "btrfs scrub status --raw /mnt/storage | grep -Eq 'Status:[[:space:]]+finished'",
        timeout=300,
    )
    assert scrub_anchor(concurrency) == running_anchor, (
        "the scrub that finished must be the one that was already running; "
        "anchor moved from {}".format(running_anchor)
    )
    concurrency.fail("systemctl is-failed {}".format(SERVICE))
    concurrency.fail("test -f /var/lib/braid/scrub-failed")
    result = show(concurrency, SERVICE, "Result")
    assert result == "success", (
        "a poke during a running scrub must leave Result=success, got {}".format(
            result
        )
    )

concurrency.shutdown()
