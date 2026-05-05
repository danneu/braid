# Intent: braid add must wait for an in-flight balance and succeed via --enqueue.
# Why: validates the sysfs-based exclusive op preflight + --enqueue wiring
#   end-to-end against a real kernel, not just unit test mocks. Also pins
#   the dry-run stream-routing contract during an in-flight balance:
#   the "waiting for in-flight" Info note goes on stdout via the rendered
#   Preview, and stderr stays empty.
# Scenario: operator has a 2-disk RAID1 pool. A background balance is running
#   (RAID1 -> single) conversion to create observable work. Operator runs
#   `braid add disk3`. Braid detects the active balance via sysfs, prints
#   a "waiting" message, and --enqueue blocks until the balance finishes.
#   The add then succeeds and disk3 appears in the pool. A follow-up
#   `braid add disk4 --dry-run` during a second balance proves the dry-run
#   contract (stdout-only, preview step lines still present).

import shlex
import time

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add_cmd(key):
    pq = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {pq} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 {key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes"
    )


# 1. Create 2-disk RAID1 pool
with subtest("create 2-disk pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed("mountpoint -q /mnt/storage")

# 2. Write data so balance has observable work
with subtest("write test data"):
    machine.succeed("dd if=/dev/urandom of=/mnt/storage/bigfile bs=1M count=512")
    machine.succeed("sync")

# 3. Start a background balance, synchronize on observed running state,
#    then run braid add and verify it waits and succeeds.
with subtest("braid add waits for balance and succeeds"):
    # Start balance in background (RAID1 → single conversion gives real work)
    machine.execute(
        "btrfs balance start -dconvert=single -mconvert=dup -f /mnt/storage "
        "> /tmp/balance.log 2>&1 &"
    )

    # Synchronize: poll until balance is confirmed running.
    # Drive decisions from observed kernel state, not timing assumptions.
    # The test MUST observe an active balance — otherwise it degrades to
    # testing the no-contention path, which existing tests already cover.
    saw_running = False
    for i in range(200):
        ret = machine.execute("btrfs balance status /mnt/storage")
        if "running" in ret[1].lower():
            saw_running = True
            break
        time.sleep(0.05)

    assert saw_running, (
        "Never observed balance in 'running' state — test cannot exercise "
        "the sysfs preflight + --enqueue wait path"
    )

    # Run braid add disk3. The balance is running, so braid will detect it
    # via sysfs, print a wait message, and --enqueue will block until the
    # balance finishes.
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

# 4. dry-run stream-routing regression: during an in-flight balance,
#    `braid add --dry-run` must put the "waiting for in-flight" Info note
#    on stdout (via the rendered Preview) and keep stderr empty, matching
#    the project-wide dry-run contract (exactly one rendered Preview on
#    stdout, stderr silent). Pre-fix, preflight wrote the wait message
#    directly to stderr and leaked it during dry-run -- a regression that
#    returned that eprintln! would fail this subtest.
with subtest("dry-run during in-flight balance routes notes to stdout only"):
    # Start another background balance so the add --dry-run path
    # observes an active exclusive op. braid add disk3 has restored RAID1,
    # so convert RAID1 -> single/DUP to produce real work.
    machine.execute(
        "btrfs balance start -dconvert=single -mconvert=dup -f /mnt/storage "
        "> /tmp/balance2.log 2>&1 &"
    )

    saw_running = False
    for i in range(200):
        ret = machine.execute("btrfs balance status /mnt/storage")
        if "running" in ret[1].lower():
            saw_running = True
            break
        time.sleep(0.05)

    assert saw_running, (
        "Never observed second balance in 'running' state -- "
        "dry-run subtest cannot exercise the preflight busy-op branch"
    )

    # Capture stdout and stderr separately. `braid add disk4 --dry-run`
    # must succeed (dry-run never mutates), emit the "waiting for
    # in-flight" Info note on stdout, leave stderr empty, and still
    # render preview step lines on stdout.
    pq = shlex.quote(passphrase)
    dry_cmd = (
        f"printf '%s\\n' {pq} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 disk4=/dev/disk/by-id/virtio-disk4 "
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
    # `[safe       ]` / `[destructive]` / `[long       ]`. Assert at
    # least one bracketed risk tag is present on stdout to prove the
    # step block still rendered after the Info note.
    assert "[safe" in stdout or "[destructive" in stdout or "[long" in stdout, (
        f"expected at least one bracketed step risk tag on dry-run stdout, got:\n{stdout}"
    )

machine.shutdown()
