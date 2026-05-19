# Test: execstop-cleans-stale-online
#
# Intent:
#   `braid lock --systemd-stop` must run full lock cleanup when
#   braid-online.service is stale after an out-of-band unmount.
#
# Why it exists:
#   A mountpoint-only fast path would skip orphan mapper cleanup, leaving
#   /dev/mapper/braid-* open while systemd believes the pool is offline.
#
# Scenario:
#   The pool is unlocked, someone unmounts /mnt/storage directly, then
#   systemd stops braid-online.service. ExecStop must close the remaining
#   braid-owned LUKS mappers.

import shlex

start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

passphrase = "testpassphrase"
pq = shlex.quote(passphrase)

with subtest("Unlock pool and leave braid-online active"):
    machine.succeed(f"printf %s\\\\n {pq} | braid unlock --passphrase-stdin")
    machine.succeed("mountpoint -q /mnt/storage")
    machine.wait_until_succeeds("systemctl is-active --quiet braid-online.service", timeout=30)

with subtest("Out-of-band unmount leaves mappers open"):
    machine.succeed("umount /mnt/storage")
    machine.fail("mountpoint -q /mnt/storage")
    machine.succeed("ls /dev/mapper/braid-*")
    machine.succeed("systemctl is-active --quiet braid-online.service")

with subtest("ExecStop closes orphan mappers"):
    machine.succeed("systemctl stop braid-online.service")
    machine.fail("ls /dev/mapper/braid-*")
    machine.fail("systemctl is-active --quiet braid-online.service")
