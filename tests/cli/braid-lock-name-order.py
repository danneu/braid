# Intent: braid lock prints "already closed" prelude lines in DiskName
# order, even when LUKS UUIDs sort opposite to names.
#
# Why it exists: helper unit tests do not exercise the binary's stderr
# output, where the previous UUID-order loop lived.
#
# Scenario: pool.json has two members and no live mappers; `braid lock`
# should report both as already closed in alphabetical order.

import json
import re

start_all()
machine.wait_for_unit("multi-user.target")

pool = {
    "disks": {
        "11111111-1111-1111-1111-111111111111": {
            "name": "zeta",
            "by_id": "/dev/disk/by-id/ata-Z",
        },
        "99999999-9999-9999-9999-999999999999": {
            "name": "alpha",
            "by_id": "/dev/disk/by-id/ata-A",
        },
    }
}
machine.succeed("mkdir -p /var/lib/braid")
machine.succeed("cat > /var/lib/braid/pool.json << 'EOF'\n" + json.dumps(pool) + "\nEOF")

out = machine.succeed("braid lock 2>&1")
order = [m.group(1) for m in re.finditer(r"disk (\S+): already closed", out)]
assert order == ["alpha", "zeta"], (
    "lock already-closed prelude must be in DiskName order, got: " + str(order)
)

machine.shutdown()
