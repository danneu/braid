# Test: fan-control-disabled
#
# Intent: Verify that /etc/braid/config.json omits the `fan_control` key
# entirely when braid.fanControl.enable = false.
#
# Why it exists: the CLI uses key presence as the enable signal. A
# regression that emits a default-zeroed fan_control block would silently
# enable the Fans section for every user who never opted in.
#
# Scenario: NixOS VM with braid enabled but fanControl disabled. Assert
# `mount_point` is present and `fan_control` is not.

import json

start_all()
machine.wait_for_unit("multi-user.target")

with subtest("braid CLI config.json omits fan_control when disabled"):
    cfg = json.loads(machine.succeed("cat /etc/braid/config.json"))
    assert cfg["mount_point"] == "/mnt/storage"
    assert "fan_control" not in cfg, (
        f"fan_control key should be absent when enable = false; got: {cfg}"
    )

machine.shutdown()
