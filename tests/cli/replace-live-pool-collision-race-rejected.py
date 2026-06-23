# Intent:
#   `braid replace` rejects an ExistingLuks replacement target when a clone of
#   that target's LUKS UUID enters the mounted pool between confirmation and
#   journal write.
#
# Why it exists:
#   Planning-time UUID checks cannot see devices added while replace waits for
#   operator confirmation and passphrase input. The execute-time live-pool
#   re-probe must close that window before pending-op.json or
#   `btrfs replace start`.
#
# Scenario:
#   Operator prepares disk3 as the replacement for disk2 and starts
#   interactive `braid replace`. While it waits at confirmation, disk3's LUKS
#   header is cloned to disk4, disk4 is opened as `clone-foreign`, and the
#   mapper is added to `/mnt/storage`. Resuming replace must fail with the
#   canonical duplicate-UUID live_pool refusal and leave braid state untouched.

import json
import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"


def luks_args():
    return (
        "--luks-format-arg=--pbkdf "
        "--luks-format-arg=pbkdf2 "
        "--luks-format-arg=--pbkdf-force-iterations "
        "--luks-format-arg=1000"
    )


def add_cmd(name):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"braid add {luks_args()} {name}=/dev/disk/by-id/virtio-{name} "
        "--passphrase-stdin --yes"
    )


def read_pool():
    raw = machine.succeed("cat /var/lib/braid/pool.json")
    return json.loads(raw)


with subtest("Build healthy pool and prepare ExistingLuks replacement target"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed("echo 'replace live-pool collision data' > /mnt/storage/kept.txt")
    machine.succeed("sync")

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for mapper in ["braid-disk1", "braid-disk2"]:
        assert f"/dev/mapper/{mapper}" in fi_show, fi_show

    passphrase_q = shlex.quote(passphrase)
    machine.succeed(
        f"printf '%s' {passphrase_q} | "
        "cryptsetup luksFormat --batch-mode --key-file=- "
        "--pbkdf pbkdf2 --pbkdf-force-iterations 1000 "
        "/dev/disk/by-id/virtio-disk3"
    )
    disk3_uuid = machine.succeed(
        "cryptsetup luksUUID /dev/disk/by-id/virtio-disk3"
    ).strip()
    assert disk3_uuid, "expected disk3 to have a LUKS UUID"
    machine.fail("test -e /dev/mapper/braid-disk3")

    pool = read_pool()
    assert "disk1" in member_names(pool), pool
    assert "disk2" in member_names(pool), pool
    assert "disk3" not in member_names(pool), pool
    machine.succeed("cp /var/lib/braid/pool.json /tmp/pool-before-replace.json")


with subtest("Add cloned disk4 to the live pool while braid replace waits"):
    machine.succeed(
        r"""cat > /tmp/clone-during-replace.sh <<'SCRIPT'
#!/bin/sh
set -eu

PASS=$1
FIFO=/tmp/braid-replace-in
OUT=/tmp/braid-replace-out
EXIT=/tmp/braid-replace-exit

rm -f "$FIFO" "$OUT" "$EXIT"
mkfifo "$FIFO"

(
  set +e
  braid replace \
    --luks-format-arg=--pbkdf \
    --luks-format-arg=pbkdf2 \
    --luks-format-arg=--pbkdf-force-iterations \
    --luks-format-arg=1000 \
    --old disk2 \
    --new disk3=/dev/disk/by-id/virtio-disk3 \
    --passphrase-stdin < "$FIFO" > "$OUT" 2>&1
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
sync

printf 'yes\n' >&3
printf '%s\n' "$PASS" >&3
exec 3>&-
wait "$BRAID_PID" || true
SCRIPT
chmod +x /tmp/clone-during-replace.sh
"""
    )
    script_exit, script_output = machine.execute(
        f"/tmp/clone-during-replace.sh {shlex.quote(passphrase)} 2>&1"
    )
    assert script_exit == 0, (
        f"clone helper failed with exit {script_exit}:\n{script_output}\n"
        f"braid output:\n{machine.execute('cat /tmp/braid-replace-out 2>&1')[1]}"
    )

    disk4_uuid = machine.succeed(
        "cryptsetup luksUUID /dev/disk/by-id/virtio-disk4"
    ).strip()
    assert disk4_uuid == disk3_uuid, f"expected cloned UUID, got {disk4_uuid}"
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "/dev/mapper/clone-foreign" in fi_show, fi_show


with subtest("braid replace fails with the canonical live_pool refusal"):
    braid_exit = int(machine.succeed("cat /tmp/braid-replace-exit").strip())
    braid_out = machine.succeed("cat /tmp/braid-replace-out")
    assert braid_exit != 0, f"replace must refuse the live clone:\n{braid_out}"
    for needle in [
        "duplicate LUKS UUID",
        disk3_uuid,
        "already present in live_pool",
    ]:
        assert needle in braid_out, f"missing {needle!r} in:\n{braid_out}"

    machine.fail("test -e /var/lib/braid/pending-op.json")
    machine.succeed("cmp /tmp/pool-before-replace.json /var/lib/braid/pool.json")


machine.shutdown()
