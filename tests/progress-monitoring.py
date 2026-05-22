# Observe and capture in-progress btrfs output during balance, scrub, and device-remove.
#
# Starts operations in background on larger disks, polls progress commands
# inside VM shell loops (to avoid test-driver round-trip latency), and asserts
# at least one in-progress sample is captured before each operation completes.
# Captured samples become golden fixtures for Rust parser tests.
#
# This test does NOT assert end-to-end completion of device-remove — that is
# covered by btrfs-shrink and braid-remove-disk tests. We exit as soon as
# in-progress fixtures are captured to keep build times short.

PASSPHRASE = "testpassphrase"
MOUNT = "/mnt/storage"
FIXTURE_DIR = "/tmp/fixtures"
DISK3_DM = "disk3-delay"


start_all()
machine.wait_for_unit("multi-user.target")

# LUKS format + open disk1 and disk2 directly
for name in ["disk1", "disk2"]:
    dev = f"/dev/disk/by-id/virtio-{name}"
    machine.succeed(
        f"echo -n '{PASSPHRASE}' | cryptsetup luksFormat --batch-mode --key-file=- "
        f"--pbkdf pbkdf2 --pbkdf-force-iterations 1000 {dev}"
    )
    machine.succeed(
        f"echo -n '{PASSPHRASE}' | cryptsetup luksOpen --key-file=- {dev} {name}"
    )

# disk3: dm-delay wrapper (0ms initially) → LUKS on top
dm_delay_create(machine, "disk3")
machine.succeed(
    f"echo -n '{PASSPHRASE}' | cryptsetup luksFormat --batch-mode --key-file=- "
    f"--pbkdf pbkdf2 --pbkdf-force-iterations 1000 /dev/mapper/{DISK3_DM}"
)
machine.succeed(
    f"echo -n '{PASSPHRASE}' | cryptsetup luksOpen --key-file=- /dev/mapper/{DISK3_DM} disk3"
)

# Create single-profile btrfs on disk1 only, mount, add disk2
machine.succeed("mkfs.btrfs -f -d single -m dup /dev/mapper/disk1")
machine.succeed(f"mkdir -p {MOUNT}")
machine.succeed(f"mount /dev/mapper/disk1 {MOUNT}")
machine.succeed(f"btrfs device add -f /dev/mapper/disk2 {MOUNT}")
machine.succeed(f"mkdir -p {FIXTURE_DIR}")


with subtest("balance progress observed"):
    # Write heavy workload (~2 GiB) so balance takes observable time
    machine.succeed(f"dd if=/dev/urandom of={MOUNT}/bigfile bs=1M count=512")
    machine.succeed("sync")

    # Start balance in background and poll in the same shell command
    # to eliminate host round-trip latency — balance can finish in ~5s
    # on fast VM I/O, so any gap between start and poll risks missing it.
    machine.succeed(
        f"btrfs balance start -dconvert=raid1 -mconvert=raid1 {MOUNT} "
        f"> /tmp/balance-start.log 2>&1 < /dev/null & "
        f"for i in $(seq 1 300); do "
        f"out=\"$(btrfs balance status {MOUNT} 2>&1 || true)\"; "
        f"if printf '%s\\n' \"$out\" | grep -Eq 'is (running|paused)'; then "
        f"printf '%s\\n' \"$out\" > {FIXTURE_DIR}/btrfs-balance-status-running.txt; "
        f"exit 0; fi; sleep 0.05; done; exit 1"
    )

    # Wait for balance to finish
    machine.wait_until_succeeds(
        f"btrfs balance status {MOUNT} | grep -q 'No balance found'",
        timeout=300,
    )

    # Verify RAID1 profile
    df_output = machine.succeed(f"btrfs fi df {MOUNT}")
    assert "RAID1" in df_output, f"Expected RAID1 in btrfs fi df:\n{df_output}"


# awk program to sum allocation bytes for disk3 from btrfs device usage --raw
# (sums Type,Profile lines like "   Data,RAID1:  67108864" within the disk3 block)
AWK_DISK3_BYTES = (
    "awk '"
    "/\\/dev\\/mapper\\/disk3, ID:/{f=1;next} "
    "f && !/^[ \\t]/ && /./{exit} "
    "f && /^[ \\t]/ && /,.*:/{split($0,a,\":\");gsub(/[^0-9]/,\"\",a[2]);t+=a[2]} "
    "END{print t+0}'"
)


# Add disk3 to pool and spread data across all 3 devices.
# Shared setup for scrub and device-remove subtests.
machine.succeed(f"btrfs device add -f /dev/mapper/disk3 {MOUNT}")
machine.succeed(
    f"btrfs balance start -dconvert=raid1 -mconvert=raid1 {MOUNT}"
)
machine.succeed(f"dd if=/dev/urandom of={MOUNT}/bigfile2 bs=1M count=1024")
machine.succeed("sync")


with subtest("scrub per-device progress observed"):
    # Inject 50ms read delay on disk3 so scrub takes observable time.
    # Without this, VM I/O is so fast the scrub finishes before a single poll.
    # 5ms is insufficient (~3s scrub); 50ms stretches disk3's scrub to ~10s+.
    dm_delay_activate(machine, "disk3", read_delay_ms=50)

    # Start scrub in background and poll in the same shell command
    # to eliminate host round-trip latency.
    # Capture both per-device (-d -R) and aggregate (--raw) running fixtures.
    machine.succeed(
        f"btrfs scrub start {MOUNT} "
        f"> /dev/null 2>&1 < /dev/null & "
        f"for i in $(seq 1 600); do "
        f"per_dev_out=\"$(btrfs scrub status -d -R {MOUNT} 2>&1 || true)\"; "
        f"agg_out=\"$(btrfs scrub status --raw {MOUNT} 2>&1 || true)\"; "
        f"if printf '%s\\n' \"$per_dev_out\" | grep -q 'Status:.*running' "
        f"&& printf '%s\\n' \"$agg_out\" | grep -q 'Status:.*running'; then "
        f"printf '%s\\n' \"$per_dev_out\" > {FIXTURE_DIR}/btrfs-scrub-per-device-running.txt; "
        f"printf '%s\\n' \"$agg_out\" > {FIXTURE_DIR}/btrfs-scrub-running.txt; "
        f"exit 0; fi; sleep 0.05; done; exit 1"
    )
    dm_delay_deactivate(machine, "disk3")

    # Wait for scrub to finish
    machine.wait_until_succeeds(
        f"btrfs scrub status {MOUNT} | grep -q 'finished'",
        timeout=300,
    )

    # Capture finished state
    machine.succeed(
        f"btrfs scrub status -d -R {MOUNT}"
        f" > {FIXTURE_DIR}/btrfs-scrub-per-device-finished.txt"
    )

    # Remove read delay before device-remove subtest


with subtest("device remove progress observed"):
    # Record initial disk3 allocation bytes
    machine.succeed(
        f"btrfs device usage --raw {MOUNT} | {AWK_DISK3_BYTES} > /tmp/disk3-initial-bytes"
    )
    initial_bytes = int(machine.succeed("cat /tmp/disk3-initial-bytes").strip())
    assert initial_bytes > 0, f"disk3 should have allocations, got {initial_bytes}"

    # Inject 20ms write-only delay on disk3 to slow block group relocation
    # enough for the polling loop to observe bytes decreasing.  Write-only
    # so that btrfs device usage reads remain fast.  Without this, VM I/O
    # is so fast the remove completes before a single poll fires.
    dm_delay_activate(machine, "disk3", write_delay_ms=20)

    # Start device remove in background and poll in the same shell command
    # to eliminate host round-trip latency — the remove can finish in ~3s
    # on fast VM I/O, so any gap between start and poll risks missing it.
    machine.succeed(
        f"btrfs device remove /dev/mapper/disk3 {MOUNT} "
        f"> /dev/null 2>&1 < /dev/null & "
        f"initial=$(cat /tmp/disk3-initial-bytes); "
        f"for i in $(seq 1 1200); do "
        f"out=\"$(btrfs device usage --raw {MOUNT} 2>&1)\" || {{ sleep 0.05; continue; }}; "
        f"if ! printf '%s\\n' \"$out\" | grep -q '/dev/mapper/disk3'; then exit 1; fi; "
        f"current=$(printf '%s\\n' \"$out\" | {AWK_DISK3_BYTES}); "
        f"if [ \"$current\" -lt \"$initial\" ]; then "
        f"printf '%s\\n' \"$out\" > {FIXTURE_DIR}/btrfs-device-usage-removing.txt; "
        f"exit 0; fi; sleep 0.05; done; exit 1"
    )
    dm_delay_deactivate(machine, "disk3")

    # No completion wait — we only need the in-progress fixture.
    # Full device-remove correctness is covered by btrfs-shrink and
    # braid-remove-disk tests.


# Copy fixtures out of the VM
machine.copy_from_vm(FIXTURE_DIR, "")
