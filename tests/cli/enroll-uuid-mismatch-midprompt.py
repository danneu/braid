# Intent: `braid enroll --generate --passphrase-stdin` rejects a member
# whose LUKS UUID changes after discovery but before execute-time mutation.
#
# Why it exists: enroll's post-passphrase UUID re-probe is the boundary guard
# that prevents slot 1 from being written to a foreign LUKS container.
#
# Scenario: operator starts enroll on a locked 2-disk pool. While braid waits
# for the passphrase, disk2 is reformatted behind the same by-id path. Resuming
# enroll must fail before creating braid.key or mutating any slot 1.

import base64
import json
import re
import shlex


start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"


def add_cmd(key):
    pq = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {pq} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 {key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes"
    )


def unlock_cmd():
    pq = shlex.quote(passphrase)
    return f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin"


def close_all():
    machine.execute("umount /mnt/storage 2>/dev/null || true")
    for key in ["disk1", "disk2"]:
        machine.execute(f"cryptsetup close braid-{key} 2>/dev/null || true")


def assert_slot1_empty(device):
    dump = machine.succeed(f"cryptsetup luksDump --dump-json-metadata {device}")
    assert '"1"' not in dump, f"slot 1 should be empty on {device}:\n{dump}"


def assert_mismatch_output(output, old_uuid, new_uuid):
    assert "LUKS UUID mismatch" in output, (
        f"expected UUID mismatch in output, got: {output}"
    )
    assert "detach the foreign disk" in output, (
        f"expected remediation hint in output, got: {output}"
    )
    assert "braid replace" in output, (
        f"expected replacement command in output, got: {output}"
    )
    assert "disk2" in output, f"expected disk2 in output, got: {output}"
    assert old_uuid in output, (
        f"expected original UUID {old_uuid} in output, got: {output}"
    )
    assert new_uuid in output, f"expected new UUID {new_uuid} in output, got: {output}"


def install_cryptsetup_logger():
    real_cryptsetup = machine.succeed("command -v cryptsetup").strip()
    wrapper_template = """#!/bin/sh
__REAL_CRYPTSETUP__ "$@"
rc=$?
printf '%s\\n' "$*" >> /tmp/cs.log
exit "$rc"
"""
    wrapper_script = wrapper_template.replace("__REAL_CRYPTSETUP__", real_cryptsetup)
    wrapper_b64 = base64.b64encode(wrapper_script.encode()).decode()
    machine.succeed(
        "rm -rf /tmp/shim && mkdir -p /tmp/shim && "
        f"printf '%s' {shlex.quote(wrapper_b64)} | base64 -d > /tmp/shim/cryptsetup && "
        "chmod +x /tmp/shim/cryptsetup"
    )


with subtest("Setup: create locked pool and mounted keyfile target"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))

    close_all()
    machine.succeed(unlock_cmd())
    machine.succeed("mountpoint -q /mnt/storage")

    pool_raw = machine.succeed("cat /var/lib/braid/pool.json")
    pool = json.loads(pool_raw)
    old_uuid = member_uuid(pool, "disk2")

    close_all()

    machine.succeed("mkdir -p /tmp/usb")
    machine.succeed("mount -t tmpfs -o size=1m,mode=700 tmpfs /tmp/usb")
    machine.succeed("mountpoint -q /tmp/usb")


with subtest("Setup: resolve unwrapped braid and install cryptsetup logger"):
    braid_wrapped_path = machine.succeed("readlink -f $(command -v braid)").strip()
    wrapper_source = machine.succeed(f"cat {braid_wrapped_path}")
    m = re.search(r'(/nix/store/[^"\s]+/bin/braid)(?!\-)', wrapper_source)
    assert m, f"could not locate unwrapped braid in wrapper:\n{wrapper_source}"
    unwrapped_braid = m.group(1)
    install_cryptsetup_logger()


with subtest("Swap disk2 after discovery while enroll waits for passphrase"):
    script_template = r"""cat > /tmp/swap-during-enroll.sh <<'SCRIPT'
#!/bin/sh
set -eu

PASS=$1
UNWRAPPED=__UNWRAPPED_BRAID__
FIFO=/tmp/braid-enroll-in
OUT=/tmp/braid-enroll-out
EXIT=/tmp/braid-enroll-exit
LOG=/tmp/cs.log

rm -f "$FIFO" "$OUT" "$EXIT"
mkfifo "$FIFO"
: > "$LOG"

(
  set +e
  PATH=/tmp/shim:$PATH "$UNWRAPPED" enroll /tmp/usb --generate --passphrase-stdin < "$FIFO" > "$OUT" 2>&1
  printf '%s\n' "$?" > "$EXIT"
) &
BRAID_PID=$!

exec 3>"$FIFO"

status_seen=0
i=0
while [ "$i" -lt 300 ]; do
  if grep -qx "status braid-disk2" "$LOG" 2>/dev/null; then
    status_seen=1
    break
  fi
  if ! kill -0 "$BRAID_PID" 2>/dev/null; then
    wait "$BRAID_PID" || true
    echo "braid exited before disk2 discovery completed" >&2
    cat "$OUT" >&2 || true
    cat "$LOG" >&2 || true
    exit 1
  fi
  i=$((i + 1))
  sleep 0.1
done

if [ "$status_seen" -ne 1 ]; then
  echo "timed out waiting for disk2 discovery status call" >&2
  cat "$OUT" >&2 || true
  cat "$LOG" >&2 || true
  kill "$BRAID_PID" 2>/dev/null || true
  wait "$BRAID_PID" || true
  exit 1
fi

if ! grep -qx "luksUUID /dev/disk/by-id/virtio-disk2" "$LOG" 2>/dev/null; then
  echo "disk2 discovery did not log a matching luksUUID probe before the gate" >&2
  cat "$LOG" >&2 || true
  kill "$BRAID_PID" 2>/dev/null || true
  wait "$BRAID_PID" || true
  exit 1
fi

if ! kill -0 "$BRAID_PID" 2>/dev/null; then
  wait "$BRAID_PID" || true
  echo "braid exited before passphrase release" >&2
  cat "$OUT" >&2 || true
  exit 1
fi

printf '%s' "$PASS" | cryptsetup luksFormat --batch-mode --key-file=- --pbkdf pbkdf2 --pbkdf-force-iterations 1000 /dev/disk/by-id/virtio-disk2

if ! kill -0 "$BRAID_PID" 2>/dev/null; then
  wait "$BRAID_PID" || true
  echo "braid observed the swap before passphrase release" >&2
  cat "$OUT" >&2 || true
  exit 1
fi

printf '%s\n' "$PASS" >&3
exec 3>&-
wait "$BRAID_PID" || true
SCRIPT
chmod +x /tmp/swap-during-enroll.sh
"""
    script_cmd = script_template.replace(
        "__UNWRAPPED_BRAID__", shlex.quote(unwrapped_braid)
    )
    machine.succeed(script_cmd)
    script_exit, script_output = machine.execute(
        f"/tmp/swap-during-enroll.sh {shlex.quote(passphrase)} 2>&1"
    )
    assert script_exit == 0, (
        f"swap helper failed with exit {script_exit}:\n{script_output}\n"
        f"braid output:\n{machine.execute('cat /tmp/braid-enroll-out 2>&1')[1]}\n"
        f"cryptsetup log:\n{machine.execute('cat /tmp/cs.log 2>&1')[1]}"
    )

    new_uuid = machine.succeed(
        "cryptsetup luksUUID /dev/disk/by-id/virtio-disk2"
    ).strip()
    assert new_uuid != old_uuid, (
        f"reformat should produce a different UUID; old={old_uuid} new={new_uuid}"
    )


with subtest("braid enroll fails closed at the execute-time re-probe"):
    braid_exit = int(machine.succeed("cat /tmp/braid-enroll-exit").strip())
    braid_out = machine.succeed("cat /tmp/braid-enroll-out")
    assert braid_exit != 0, f"enroll must refuse on UUID mismatch:\n{braid_out}"
    assert_mismatch_output(braid_out, old_uuid, new_uuid)

    cs_log = machine.succeed("cat /tmp/cs.log")
    disk2_luksuuid_count = cs_log.count("luksUUID /dev/disk/by-id/virtio-disk2\n")
    assert disk2_luksuuid_count == 2, (
        f"expected discovery and execute re-probe for disk2, got {disk2_luksuuid_count}:\n{cs_log}"
    )

    machine.fail("test -f /tmp/usb/braid.key")
    assert_slot1_empty("/dev/disk/by-id/virtio-disk1")
    assert_slot1_empty("/dev/disk/by-id/virtio-disk2")


machine.shutdown()
