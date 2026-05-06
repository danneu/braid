# Test: confirm prompt and passphrase share one stdin stream
#
# Intent:
#   `braid add --passphrase-stdin` and `braid replace --passphrase-stdin`
#   without `--yes` consume "yes\n" for the confirmation prompt and
#   "secret\n" for the passphrase read from a single piped stdin.
#
# Why it exists:
#   `std::io::stdin().lock()` is buffered. If confirmation uses that reader,
#   it can drain both lines from fd 0, stash the passphrase line in std's
#   process-lifetime buffer, and leave the later passphrase read at EOF.
#
# Scenario:
#   Operator automates a confirmation prompt and passphrase prompt with one
#   pipe, as in `printf 'yes\nsecret\n' | braid add --passphrase-stdin`.

import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "secret"


def luks_args():
    return (
        "--luks-format-arg=--pbkdf "
        "--luks-format-arg=pbkdf2 "
        "--luks-format-arg=--pbkdf-force-iterations "
        "--luks-format-arg=1000"
    )


def confirm_and_passphrase_prefix():
    pp = shlex.quote(passphrase)
    return "printf 'yes\\n%s\\n' " + pp + " | "


def add_cmd(name):
    return (
        confirm_and_passphrase_prefix()
        + "braid add "
        + luks_args()
        + " "
        + name
        + "=/dev/disk/by-id/virtio-"
        + name
        + " --passphrase-stdin"
    )


def replace_cmd(old, new):
    return (
        confirm_and_passphrase_prefix()
        + "braid replace "
        + luks_args()
        + " --old "
        + old
        + " --new "
        + new
        + "=/dev/disk/by-id/virtio-"
        + new
        + " --passphrase-stdin"
    )


with subtest("add consumes confirm and passphrase from one stdin stream"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in ["braid-disk1", "braid-disk2"]:
        assert "/dev/mapper/" + name in fi_show, name + " missing:\n" + fi_show

    machine.succeed("echo 'important data' > /mnt/storage/precious.txt")
    machine.succeed("sync")

with subtest("replace consumes confirm and passphrase from one stdin stream"):
    machine.succeed(replace_cmd("disk1", "disk3"))

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "/dev/mapper/braid-disk3" in fi_show, "disk3 missing:\n" + fi_show
    assert "/dev/mapper/braid-disk2" in fi_show, "disk2 missing:\n" + fi_show
    assert "braid-disk1" not in fi_show, "disk1 should be removed:\n" + fi_show

    content = machine.succeed("cat /mnt/storage/precious.txt").strip()
    assert content == "important data", "unexpected file content: " + content

machine.shutdown()
