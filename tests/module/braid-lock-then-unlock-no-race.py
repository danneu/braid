# Test: braid-lock-then-unlock-no-race
#
# Intent:
#   Plain `braid lock` must return only after braid-online.service is inactive.
#
# Why it exists:
#   If post-lock systemctl stop is queued with --no-block, a late ExecStop can
#   run after the next unlock and immediately re-lock the freshly mounted pool.
#
# Scenario:
#   Operator locks the pool and immediately unlocks it again. The second unlock
#   should remain mounted because no stale stop job is still pending.

import shlex
import time

start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

passphrase = "testpassphrase"
pq = shlex.quote(passphrase)

with subtest("Unlock pool"):
    machine.succeed(f"printf %s\\\\n {pq} | braid unlock --passphrase-stdin")
    machine.succeed("mountpoint -q /mnt/storage")
    machine.wait_until_succeeds("systemctl is-active --quiet braid-online.service", timeout=30)

with subtest("Plain braid lock leaves braid-online inactive on return"):
    machine.succeed("braid lock")
    machine.fail("mountpoint -q /mnt/storage")
    machine.fail("systemctl is-active --quiet braid-online.service")

with subtest("Immediate unlock is not undone by a late ExecStop"):
    machine.succeed(f"printf %s\\\\n {pq} | braid unlock --passphrase-stdin")
    machine.succeed("mountpoint -q /mnt/storage")
    time.sleep(3)
    machine.succeed("mountpoint -q /mnt/storage")
