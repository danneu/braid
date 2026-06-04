# Intent: braid add must wait for an in-flight balance and succeed via --enqueue,
#   and `braid add --dry-run` during an in-flight balance must route its
#   "waiting for in-flight" Info note to stdout (via the rendered Preview) while
#   leaving stderr empty.
# Why it exists: validates the sysfs-based exclusive op preflight + --enqueue
#   wiring end-to-end against a real kernel, not just unit-test mocks, and pins
#   the dry-run stream-routing contract during an in-flight balance.
# Scenario: operator has a 2-disk RAID1 pool. A background balance
#   (RAID1 -> single/DUP conversion) is running to create observable work.
#   Operator runs `braid add disk3`: braid detects the active balance via sysfs,
#   prints a "waiting" message, and --enqueue blocks until the balance finishes;
#   the add then succeeds and disk3 appears in the pool. A follow-up
#   `braid add disk4 --dry-run` during a second balance proves the dry-run
#   contract (stdout-only note, preview step lines still present).
#
#   The two initial pool members (disk1, disk2) sit behind dm-delay devices with
#   a 2000ms write delay (read_delay = 0). Every btrfs transaction commit writes
#   a superblock to every device, so the write delay keeps each balance in
#   'running' long enough for braid to finish startup and reach its preflight.
#   This removes the payload-size-vs-disk-speed race that flaked this test: on
#   fast disks the 512MB RAID1->single balance could finish before braid read
#   /sys/fs/btrfs/{fsid}/exclusive_operation, so braid saw 'none', emitted no
#   note, and the assertion failed. Reads stay fast (read_delay = 0) so braid's
#   own probe/sysfs read are not slowed -- the delay only sustains the window.

import shlex
import time

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"
DELAYED = ["disk1", "disk2"]
WRITE_DELAY_MS = 2000  # read_delay stays 0 so braid's probe/sysfs read stay fast


def disk_path(key):
    # disk1/disk2 route through the dm-delay symlink so their write delay gates
    # every balance; disk3/disk4 stay raw virtio.
    if key in DELAYED:
        return f"/dev/disk/by-id/braid-test-{key}-delay"
    return f"/dev/disk/by-id/virtio-{key}"


def add_cmd(key):
    pq = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {pq} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 {key}={disk_path(key)} --passphrase-stdin --yes"
    )


def wait_for_running_balance():
    # Drive decisions from observed kernel state, not timing assumptions.
    # The test MUST observe an active balance — otherwise it degrades to
    # testing the no-contention path, which existing tests already cover.
    for _ in range(200):
        ret = machine.execute("btrfs balance status /mnt/storage")
        if "running" in ret[1].lower():
            return True
        time.sleep(0.05)
    return False


# 1. Create 2-disk RAID1 pool. Put the initial members behind dm-delay devices
#    first; the delay is inactive here, so the build runs at full speed.
with subtest("create 2-disk pool"):
    for name in DELAYED:
        dm_delay_create(machine, name)
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed("mountpoint -q /mnt/storage")

# 2. Write data so balance has observable work (delay still inactive -> fast write)
with subtest("write test data"):
    machine.succeed("dd if=/dev/urandom of=/mnt/storage/bigfile bs=1M count=512")
    machine.succeed("sync")

# 3. Activate the write delay, start a background balance, synchronize on the
#    observed running state, then run braid add and verify it waits and
#    succeeds. The write delay keeps the balance in 'running' long enough that
#    braid reaches its sysfs preflight before the relocation loop clears the
#    exclusive op.
with subtest("braid add waits for balance and succeeds"):
    dm_delay_activate(machine, DELAYED, write_delay_ms=WRITE_DELAY_MS)

    # Start balance in background (RAID1 → single conversion gives real work)
    machine.execute(
        "btrfs balance start -dconvert=single -mconvert=dup -f /mnt/storage "
        "> /tmp/balance.log 2>&1 &"
    )

    assert wait_for_running_balance(), (
        "Never observed balance in 'running' state — test cannot exercise "
        "the sysfs preflight + --enqueue wait path"
    )

    # Run braid add disk3. The balance is running, so braid will detect it
    # via sysfs, print a wait message, and --enqueue will block until the
    # balance finishes. disk3 is raw virtio; the delay on disk1/disk2 is what
    # keeps the balance alive.
    result = machine.execute(add_cmd("disk3") + " 2>&1")
    exit_code = result[0]
    output = result[1]

    assert exit_code == 0, f"braid add disk3 failed (exit {exit_code}):\n{output}"

    # The wait message proves braid saw the active op via sysfs and proceeded
    # with --enqueue rather than erroring or skipping the check.
    assert "waiting for in-flight" in output.lower(), (
        f"expected 'waiting for in-flight' message in output:\n{output}"
    )

    # Verify disk3 is in the pool
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "/dev/mapper/braid-disk3" in fi_show, (
        f"braid-disk3 missing from pool:\n{fi_show}"
    )

    # Deactivate only after the add returns: braid's internal post-add
    # balance -dconvert=raid1 must run through the delay to keep the add
    # faithfully synchronous, and the pool is idle once the add exits, so this
    # suspend flushes nothing.
    dm_delay_deactivate(machine, DELAYED)

# 4. dry-run stream-routing regression: during an in-flight balance,
#    `braid add --dry-run` must put the "waiting for in-flight" Info note
#    on stdout (via the rendered Preview) and keep stderr empty, matching
#    the project-wide dry-run contract (exactly one rendered Preview on
#    stdout, stderr silent). Pre-fix, preflight wrote the wait message
#    directly to stderr and leaked it during dry-run -- a regression that
#    returned that eprintln! would fail this subtest.
with subtest("dry-run during in-flight balance routes notes to stdout only"):
    dm_delay_activate(machine, DELAYED, write_delay_ms=WRITE_DELAY_MS)

    # Start another background balance so the add --dry-run path observes an
    # active exclusive op. braid add disk3 restored RAID1, so convert
    # RAID1 -> single/DUP to produce real work.
    machine.execute(
        "btrfs balance start -dconvert=single -mconvert=dup -f /mnt/storage "
        "> /tmp/balance2.log 2>&1 &"
    )

    assert wait_for_running_balance(), (
        "Never observed second balance in 'running' state -- "
        "dry-run subtest cannot exercise the preflight busy-op branch"
    )

    # Capture stdout and stderr separately. `braid add disk4 --dry-run`
    # must succeed (dry-run never mutates), emit the "waiting for
    # in-flight" Info note on stdout, leave stderr empty, and still
    # render preview step lines on stdout. disk4 is raw virtio.
    pq = shlex.quote(passphrase)
    dry_cmd = (
        f"printf '%s\\n' {pq} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 disk4={disk_path('disk4')} "
        f"--passphrase-stdin --yes --dry-run "
        f"> /tmp/add-dryrun.out 2> /tmp/add-dryrun.err"
    )
    machine.succeed(dry_cmd)

    stdout = machine.succeed("cat /tmp/add-dryrun.out")
    stderr = machine.succeed("cat /tmp/add-dryrun.err")

    assert stderr == "", (
        f"dry-run stderr must be empty during busy-op preflight, got:\n{stderr!r}"
    )
    assert "waiting for in-flight" in stdout.lower(), (
        f"expected 'waiting for in-flight' on dry-run stdout, got:\n{stdout}"
    )
    # Preview step lines use the bracketed risk tag, e.g.
    # `[safe]` / `[destructive]` / `[long]`. Assert at
    # least one bracketed risk tag is present on stdout to prove the
    # step block still rendered after the Info note.
    assert "[safe" in stdout or "[destructive" in stdout or "[long" in stdout, (
        f"expected at least one bracketed step risk tag on dry-run stdout, got:\n{stdout}"
    )

    # Teardown: cancel the balance, confirm it stopped, then drop the delay.
    # Order is cleanliness, not correctness -- dm-delay's presuspend flushes
    # pending delayed bios immediately, so deactivating mid-balance is equally
    # safe and never stalls for WRITE_DELAY_MS.
    machine.execute("btrfs balance cancel /mnt/storage 2>/dev/null || true")
    for _ in range(200):
        ret = machine.execute("btrfs balance status /mnt/storage")
        if "no balance" in ret[1].lower():
            break
        time.sleep(0.1)
    dm_delay_deactivate(machine, DELAYED)

machine.shutdown()
