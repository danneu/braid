# Test braid-doctor UPS-adjacent checks against a live NUT stack.
#
# Sets up two scenarios:
#
# Scenario A (baseline happy path): unlock the pool via `braid unlock`
# so braid-online.service activates. Both UPS-adjacent doctor checks
# should be Ok.
#
# Scenario B (critical fault): unlock, then mount the pool manually
# while braid-online.service stays inactive. This is the production
# failure mode the check exists to catch -- a pool that is mounted
# but without braid-online.service active means SHUTDOWNCMD will
# poweroff without calling `braid lock`'s ExecStop. The check must
# fire with Fail severity.
#
# Note: we cannot `systemctl stop braid-online.service` to simulate
# the fault; the unit's ExecStop runs `braid lock`, which unmounts.
# So we open LUKS + mount directly, bypassing braid-online.
#
# See docs/design/decisions/020-ups-integration.md "braid-online becomes
# safety-critical under UPS" for the rationale.

import json


def find_check(report, name):
    for c in report["checks"]:
        if c["name"] == name:
            return c
    raise AssertionError(f"check {name!r} not found in: {report}")


start_all()
machine.wait_for_unit("multi-user.target")
machine.wait_for_unit("upsd.service")
machine.wait_for_unit("upsmon.service")
machine.wait_until_succeeds("upsc ups@localhost ups.status", timeout=60)

# --- Scenario A: unlock via braid and confirm both checks pass ---
machine.succeed("systemctl start braid-unlock.service")
machine.wait_until_succeeds("mountpoint -q /mnt/storage", timeout=30)
machine.wait_for_unit("braid-online.service")

raw = machine.succeed("braid doctor --json")
report = json.loads(raw)
assert find_check(report, "ups_daemon")["status"] == "ok", find_check(
    report, "ups_daemon"
)
assert (
    find_check(report, "braid_online_active")["status"] == "ok"
), find_check(report, "braid_online_active")

# --- Scenario B: simulate mounted-but-braid-online-inactive ---
#
# `systemctl stop braid-online.service` would trigger its ExecStop
# (`braid lock`) and unmount the pool -- not the fault we want to
# exercise. Instead, tear the whole pool down via `braid lock`, then
# re-mount the btrfs filesystem manually without going through
# `braid unlock`. That leaves `braid-online.service` inactive while
# the pool is mounted, exactly the configuration the check is
# supposed to catch.
machine.succeed("braid lock")
machine.wait_until_fails("mountpoint -q /mnt/storage", timeout=15)

passphrase = "testpassphrase"
# echo -n to match the passphrase bytes the initrd fixture fed to
# luksFormat (no trailing newline).
machine.succeed(
    f"echo -n '{passphrase}' | cryptsetup luksOpen "
    "--key-file=- /dev/disk/by-id/virtio-disk1 braid-disk1"
)
machine.succeed(
    "mount -o noatime,skip_balance,subvolid=5 "
    "/dev/mapper/braid-disk1 /mnt/storage"
)
machine.succeed("mountpoint -q /mnt/storage")
# Confirm braid-online is NOT active in this scenario -- otherwise
# the test is passing for the wrong reason.
machine.fail("systemctl is-active --quiet braid-online.service")

# Doctor should flip braid_online_active to Fail and tolerate non-zero
# exit (doctor exits non-zero when any check fails).
exit_code, raw = machine.execute("braid doctor --json")
report = json.loads(raw)
assert exit_code != 0, (
    f"doctor must exit non-zero when a check fails: {exit_code}\n{raw}"
)
assert report["status"] == "fail", f"expected overall fail:\n{raw}"
bo = find_check(report, "braid_online_active")
assert bo["status"] == "fail", f"expected Fail on braid_online_active, got: {bo}"
assert "UPS shutdown" in bo["message"], bo["message"]

# ups_daemon should still be Ok -- it's independent of braid-online.
assert find_check(report, "ups_daemon")["status"] == "ok", find_check(
    report, "ups_daemon"
)
