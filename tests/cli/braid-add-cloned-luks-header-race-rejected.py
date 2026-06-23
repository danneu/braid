# Intent: `braid add` rejects a returning-disk add when a cloned LUKS header
# is added to the live pool between the confirmation prompt and the
# irreversible pool-add step.
#
# Why it exists: protects the execute-time live-pool re-classification that
# closes the plan-to-execute TOCTOU window for ClosedPresentLuks and
# OpenRecoverable targets. Without that gate, an external clone-add during the
# confirmation pause slips past the canonical "duplicate LUKS UUID" defense
# and surfaces as a non-canonical btrfs error.
#
# Scenario: disk3 is a removed-but-returnable braid disk. Operator starts
# `braid add disk3=...` without `--yes`. While braid waits at the confirmation
# prompt, an external actor clones disk3's LUKS header onto disk4, opens disk4
# under `clone-foreign`, and `btrfs device add` adds it to the pool. Feeding
# "yes\n" + passphrase to the waiting `braid add` must trigger the
# execute-time live-pool re-check and surface the canonical duplicate-UUID
# refusal, leaving pool.json and pending-op.json untouched.

import json
import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"


def add_cmd(name, yes=True):
    passphrase_q = shlex.quote(passphrase)
    yes_arg = " --yes" if yes else ""
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 "
        f"--luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 "
        f"{name}=/dev/disk/by-id/virtio-{name} --passphrase-stdin{yes_arg}"
    )


def missing_devid():
    report = json.loads(machine.succeed("braid status --json"))
    devids = report.get("missing_devids", [])
    assert len(devids) == 1, f"expected one missing devid, got {devids}"
    return str(devids[0])


with subtest("Build pool with a returnable disk3"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed(add_cmd("disk3"))
    machine.succeed("echo 'add cloned header race data' > /mnt/storage/kept.txt")
    machine.succeed("sync")

    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk3")
    machine.succeed("mount -o degraded /dev/mapper/braid-disk1 /mnt/storage")
    assert "missing" in machine.succeed("btrfs fi show /mnt/storage").lower()
    devid = missing_devid()
    machine.succeed(f"braid remove-missing --missing-id {devid} --yes")

    pool = json.loads(machine.succeed("cat /var/lib/braid/pool.json"))
    assert "disk1" in member_names(pool), pool
    assert "disk2" in member_names(pool), pool
    assert "disk3" not in member_names(pool), pool
    assert "missing" not in machine.succeed("btrfs fi show /mnt/storage").lower()
    machine.succeed("cp /var/lib/braid/pool.json /tmp/pool-before-add.json")

    disk3_uuid = machine.succeed(
        "cryptsetup luksUUID /dev/disk/by-id/virtio-disk3"
    ).strip()
    assert disk3_uuid, "expected disk3 to retain its LUKS UUID"


with subtest("Add cloned disk4 to the live pool while braid add waits"):
    machine.succeed(
        r"""cat > /tmp/clone-during-add.sh <<'SCRIPT'
#!/bin/sh
set -eu

PASS=$1
FIFO=/tmp/braid-in
OUT=/tmp/braid-out
EXIT=/tmp/braid-exit

rm -f "$FIFO" "$OUT" "$EXIT"
mkfifo "$FIFO"

(
  set +e
  braid add disk3=/dev/disk/by-id/virtio-disk3 --passphrase-stdin < "$FIFO" > "$OUT" 2>&1
  printf '%s\n' "$?" > "$EXIT"
) &
BRAID_PID=$!

exec 3>"$FIFO"

prompt_seen=0
i=0
while [ "$i" -lt 300 ]; do
  if grep -q "Type 'yes' to continue" "$OUT" 2>/dev/null; then
    prompt_seen=1
    break
  fi
  if ! kill -0 "$BRAID_PID" 2>/dev/null; then
    wait "$BRAID_PID" || true
    echo "braid exited before confirmation prompt" >&2
    cat "$OUT" >&2 || true
    exit 1
  fi
  i=$((i + 1))
  sleep 0.1
done

if [ "$prompt_seen" -ne 1 ]; then
  echo "timed out waiting for confirmation prompt" >&2
  cat "$OUT" >&2 || true
  kill "$BRAID_PID" 2>/dev/null || true
  wait "$BRAID_PID" || true
  exit 1
fi

cryptsetup luksHeaderBackup \
  --header-backup-file /tmp/disk3.hdr \
  /dev/disk/by-id/virtio-disk3
cryptsetup luksHeaderRestore --batch-mode \
  --header-backup-file /tmp/disk3.hdr \
  /dev/disk/by-id/virtio-disk4
printf '%s' "$PASS" | \
  cryptsetup open --key-file=- /dev/disk/by-id/virtio-disk4 clone-foreign
btrfs device add /dev/mapper/clone-foreign /mnt/storage

printf 'yes\n' >&3
printf '%s\n' "$PASS" >&3
exec 3>&-
wait "$BRAID_PID" || true
SCRIPT
chmod +x /tmp/clone-during-add.sh
"""
    )
    script_exit, script_output = machine.execute(
        f"/tmp/clone-during-add.sh {shlex.quote(passphrase)} 2>&1"
    )
    assert script_exit == 0, (
        f"clone helper failed with exit {script_exit}:\n{script_output}\n"
        f"braid output:\n{machine.execute('cat /tmp/braid-out 2>&1')[1]}"
    )

    disk4_uuid = machine.succeed(
        "cryptsetup luksUUID /dev/disk/by-id/virtio-disk4"
    ).strip()
    assert disk4_uuid == disk3_uuid, f"expected cloned UUID, got {disk4_uuid}"
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "/dev/mapper/clone-foreign" in fi_show, fi_show


with subtest("braid add fails with the canonical duplicate UUID refusal"):
    braid_exit = int(machine.succeed("cat /tmp/braid-exit").strip())
    braid_out = machine.succeed("cat /tmp/braid-out")
    assert braid_exit != 0, f"add must refuse the live clone:\n{braid_out}"
    # Scope-only refusal (ADR 024): the message names the real add target and
    # reports the colliding side as a device already in the live pool, deriving
    # nothing from the foreign clone-foreign mapper.
    for needle in [
        "duplicate LUKS UUID",
        "add target braid-disk3 (/dev/disk/by-id/virtio-disk3)",
        "collides with a device already in the live pool",
        disk3_uuid,
    ]:
        assert needle in braid_out, f"missing {needle!r} in:\n{braid_out}"
    # The refusal must surface nothing derived from the foreign mapper: no
    # clone-foreign handle, no double-braid- prefix, no empty by-id placeholder.
    # These are the regression guards for the pre-scope rendering bug.
    for absent in [
        "clone-foreign",
        "braid-braid",
        "(/dev/disk/by-id/)",
        "is open but backed by",
    ]:
        assert absent not in braid_out, f"unexpected {absent!r} in:\n{braid_out}"

    machine.fail("test -e /var/lib/braid/pending-op.json")
    machine.succeed("cmp /tmp/pool-before-add.json /var/lib/braid/pool.json")


machine.shutdown()
