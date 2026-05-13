# Intent: `braid add` rejects a closed returned disk whose live LUKS UUID
# changes between planning and execution, before opening the mapper.
#
# Why it exists: the ClosedPresentLuks add path caches the planning-time LUKS
# UUID, then pauses for confirmation before Pass 1. A disk swap in that window
# must fail with the canonical "LUKS UUID mismatch" wording instead of opening
# and classifying a foreign LUKS volume.
#
# Scenario: a 3-disk RAID1 pool loses disk3, removes it with remove-missing,
# then starts an interactive `braid add disk3=...`. While braid waits at the
# confirmation prompt, the same by-id path is reformatted as a fresh LUKS
# container with the same label and passphrase but a different UUID. Continuing
# the add must fail closed before journal write or mapper open.

import json
import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"


def add_cmd(key):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 {key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes"
    )


def missing_devid():
    raw = machine.succeed("braid status --json")
    report = json.loads(raw)
    devids = report.get("missing_devids", [])
    assert len(devids) == 1, f"expected one missing devid, got {devids}:\n{raw}"
    return str(devids[0])


def member_names(pool):
    return {member["name"] for member in pool["disks"].values()}


with subtest("Build 3-disk RAID1 pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed(add_cmd("disk3"))
    machine.succeed("echo 'uuid swap regression data' > /mnt/storage/kept.txt")
    machine.succeed("sync")
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in ["braid-disk1", "braid-disk2", "braid-disk3"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing:\n{fi_show}"


with subtest("Remove disk3 from pool membership but leave it braid-labeled"):
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk3")
    machine.succeed("mount -o degraded /dev/mapper/braid-disk1 /mnt/storage")
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "missing" in fi_show.lower(), f"expected missing disk3:\n{fi_show}"

    devid = missing_devid()
    machine.succeed(f"braid remove-missing --missing-id {devid} --yes")
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "missing" not in fi_show.lower(), f"missing device survived removal:\n{fi_show}"
    assert "/dev/mapper/braid-disk3" not in fi_show, f"disk3 still live:\n{fi_show}"
    machine.succeed("mountpoint -q /mnt/storage")

    pool_snapshot = machine.succeed("cat /var/lib/braid/pool.json")
    pool_json = json.loads(pool_snapshot)
    assert "disk3" not in member_names(pool_json), f"disk3 still in pool.json: {pool_json}"
    machine.succeed("cp /var/lib/braid/pool.json /tmp/pool-before-add.json")

    initial_uuid = machine.succeed(
        "cryptsetup luksUUID /dev/disk/by-id/virtio-disk3"
    ).strip()
    assert initial_uuid != "", "expected disk3 to still have a LUKS UUID"


with subtest("Swap disk3 LUKS UUID while braid add waits for confirmation"):
    machine.succeed(
        r"""cat > /tmp/swap-during-add.sh <<'SCRIPT'
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

printf '%s' "$PASS" | cryptsetup luksFormat \
  --batch-mode --label=braid-disk3 --key-file=- \
  --pbkdf pbkdf2 --pbkdf-force-iterations 1000 \
  /dev/disk/by-id/virtio-disk3

printf 'yes\n' >&3
printf '%s\n' "$PASS" >&3
exec 3>&-
wait "$BRAID_PID" || true
SCRIPT
chmod +x /tmp/swap-during-add.sh
"""
    )
    script_exit, script_output = machine.execute(
        f"/tmp/swap-during-add.sh {shlex.quote(passphrase)} 2>&1"
    )
    assert script_exit == 0, (
        f"swap helper failed with exit {script_exit}:\n{script_output}\n"
        f"braid output:\n{machine.execute('cat /tmp/braid-out 2>&1')[1]}"
    )

    swapped_uuid = machine.succeed(
        "cryptsetup luksUUID /dev/disk/by-id/virtio-disk3"
    ).strip()
    assert swapped_uuid != initial_uuid, (
        f"reformat should produce a different UUID; old={initial_uuid} new={swapped_uuid}"
    )


with subtest("braid add fails closed on the UUID mismatch"):
    braid_exit = int(machine.succeed("cat /tmp/braid-exit").strip())
    braid_out = machine.succeed("cat /tmp/braid-out")
    assert braid_exit != 0, f"add must refuse on UUID mismatch:\n{braid_out}"
    for needle in [
        "add target",
        "LUKS UUID mismatch",
        f"expected {initial_uuid}",
        f"found {swapped_uuid}",
        "detach the foreign disk",
    ]:
        assert needle in braid_out, f"missing {needle!r} in:\n{braid_out}"

    machine.fail("test -e /var/lib/braid/pending-op.json")
    machine.succeed("cmp /tmp/pool-before-add.json /var/lib/braid/pool.json")

    status_exit, status_out = machine.execute("cryptsetup status braid-disk3 2>&1")
    assert status_exit != 0, f"braid-disk3 mapper should be inactive:\n{status_out}"
    assert "inactive" in status_out.lower(), (
        f"expected inactive mapper status, got:\n{status_out}"
    )


machine.shutdown()
