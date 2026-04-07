# Test: braid replace holds a logind sleep inhibitor for the duration
#
# Intent:
# - What behavior this test verifies.
#   - While `braid replace` is in flight, an inhibitor lock with what=sleep,
#     who=braid, mode=block is registered with logind, preventing the host
#     from suspending mid-replace.
#   - After the replace finishes, that inhibitor lock is released.
#
# Why it exists:
# - What risk/regression this protects against.
#   - Suspending mid-replace produces kernel-level topology corruption on
#     every kernel: the kernel resume-on-mount path finishes the data copy
#     but does not perform the post-completion devid swap, leaving a phantom
#     MISSING devid 0 in the pool (Path A in tracking issue #48). On v6.19+
#     kernels it also triggers the new freeze/signal cancellation path
#     (Path B), forcing the user to restart the replace from scratch.
#   - Upstream btrfs explicitly recommends inhibiting suspend during replace
#     — reference/btrfs-progs/Documentation/btrfs-replace.rst:49-50. braid
#     enables autosuspend by default, so this interaction is reachable in
#     normal operation.
#   - This test fails before the SleepInhibitor helper lands and passes
#     after.
#
# Scenario:
# - Real-world situation this models.
#   - Operator starts a `braid replace` to swap a healthy drive while their
#     NAS is configured with default autosuspend. autosuspend would normally
#     decide the system is idle and request a suspend partway through the
#     multi-hour replace. The inhibitor must block that suspend until the
#     replace (and its post-replace soft balance, if applicable) finishes.

import shlex
import time

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
pq = shlex.quote(passphrase)
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add_cmd(key):
    return (
        f"printf '%s\\n' {pq} | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add {key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes"
    )


def replace_cmd_bg(old, new):
    # Background the entire (passphrase | braid replace) pipeline so
    # machine.execute returns immediately. The subshell makes & apply to
    # the whole pipeline rather than just the tail braid invocation.
    return (
        f"(printf '%s\\n' {pq} | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid replace --old {old} --new {new}=/dev/disk/by-id/virtio-{new} "
        f"--passphrase-stdin --yes) > /tmp/replace.log 2>&1 &"
    )


def list_inhibitors():
    # Query logind directly via D-Bus. We avoid `systemd-inhibit --list`
    # because it depends on TTY/terminal context that NixOS VM tests do not
    # provide.
    #
    # ListInhibitors returns a(ssssuu) — an array of (what, who, why, mode,
    # uid, pid) tuples. busctl's default text output renders this as:
    #
    #   a(ssssuu) <count> "what1" "who1" "why1" "mode1" uid1 pid1 ...
    #
    # Strings containing spaces (e.g. "replace in progress") are
    # double-quoted, so shlex.split parses them correctly.
    #
    # Defensive parsing: assert the expected token shape before indexing
    # so a busctl format change fails loudly with a clear message instead
    # of an opaque IndexError or ValueError on a downstream test assert.
    out = machine.succeed(
        "busctl call org.freedesktop.login1 /org/freedesktop/login1 "
        "org.freedesktop.login1.Manager ListInhibitors"
    ).strip()
    tokens = shlex.split(out)
    assert len(tokens) >= 2, (
        f"busctl ListInhibitors output too short to parse: {out!r}"
    )
    assert tokens[0] == "a(ssssuu)", (
        f"busctl ListInhibitors returned unexpected type signature "
        f"{tokens[0]!r} (expected 'a(ssssuu)'). Output: {out!r}"
    )
    try:
        count = int(tokens[1])
    except ValueError as e:
        raise AssertionError(
            f"busctl ListInhibitors count token {tokens[1]!r} is not an int. "
            f"Output: {out!r}"
        ) from e
    expected_token_count = 2 + count * 6
    assert len(tokens) == expected_token_count, (
        f"busctl ListInhibitors token count {len(tokens)} does not match "
        f"expected {expected_token_count} for {count} inhibitor(s) "
        f"(2 header + 6-tuple per entry). Output: {out!r}"
    )
    inhibitors = []
    for i in range(count):
        base = 2 + i * 6
        try:
            uid = int(tokens[base + 4])
            pid = int(tokens[base + 5])
        except ValueError as e:
            raise AssertionError(
                f"busctl ListInhibitors uid/pid tokens at entry {i} are not "
                f"ints: {tokens[base + 4]!r} / {tokens[base + 5]!r}. "
                f"Output: {out!r}"
            ) from e
        inhibitors.append({
            "what": tokens[base],
            "who": tokens[base + 1],
            "why": tokens[base + 2],
            "mode": tokens[base + 3],
            "uid": uid,
            "pid": pid,
        })
    return inhibitors


def find_braid_sleep_inhibitor(inhibitors):
    for inh in inhibitors:
        if inh["who"] == "braid" and "sleep" in inh["what"] and inh["mode"] == "block":
            return inh
    return None


# --- Phase 1: Build a 3-disk pool and write a payload ---

with subtest("Build 3-disk pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed(add_cmd("disk3"))
    machine.succeed("mountpoint -q /mnt/storage")

with subtest("Write urandom payload"):
    # 400 MiB matches the proven sizing in the interrupted-mid-flight repro
    # test — large enough that the kernel reports in-flight progress before
    # the test driver can race past it.
    machine.succeed(
        "dd if=/dev/urandom of=/mnt/storage/payload bs=1M count=400 status=none"
    )
    machine.succeed("sync")
    payload_sha = machine.succeed("sha256sum /mnt/storage/payload").split()[0]
    print(f"payload sha256: {payload_sha}")

# --- Phase 2: Sanity-check that no braid inhibitor exists pre-replace ---

with subtest("No braid inhibitor before replace"):
    pre = list_inhibitors()
    print(f"=== inhibitors pre-replace: {pre} ===")
    assert find_braid_sleep_inhibitor(pre) is None, (
        f"unexpected pre-existing braid sleep inhibitor: {pre}"
    )

# --- Phase 3: Start replace asynchronously and wait for in-flight progress ---

with subtest("Start replace and wait for in-flight progress"):
    machine.execute(replace_cmd_bg("disk2", "disk4"))

    saw_running = False
    last_status = ""
    for _ in range(400):
        ret = machine.execute("btrfs replace status -1 /mnt/storage 2>&1")
        last_status = ret[1]
        if "Started on" in last_status or "% done" in last_status:
            saw_running = True
            break
        time.sleep(0.05)

    print("=== last btrfs replace status ===")
    print(last_status)
    assert saw_running, (
        "Never observed btrfs replace in-flight — test cannot verify the "
        "inhibitor is held during the replace. Last status:\n" + last_status
    )

# --- Phase 4: Assert the inhibitor is held while the replace is in flight ---

with subtest("braid sleep inhibitor is held during replace"):
    mid = list_inhibitors()
    print(f"=== inhibitors mid-replace: {mid} ===")
    inh = find_braid_sleep_inhibitor(mid)
    assert inh is not None, (
        "no braid sleep inhibitor found while replace is in flight. "
        f"inhibitors: {mid}"
    )
    # Bonus locks on the exact shape we expect, so a regression in any
    # field (mode flip, who rename, what widening) fails loudly.
    assert inh["what"] == "sleep", f"expected what=sleep, got {inh!r}"
    assert inh["mode"] == "block", f"expected mode=block, got {inh!r}"
    assert "replace" in inh["why"], f"expected why mentioning replace, got {inh!r}"

# --- Phase 5: Wait for the replace to finish ---

with subtest("Wait for replace to finish"):
    # `btrfs replace status -1` prints "Started on <t1>, finished on <t2>"
    # in the FINISHED state — see reference/btrfs-progs/cmds/replace.c:460.
    machine.wait_until_succeeds(
        "btrfs replace status -1 /mnt/storage 2>&1 | grep -q 'finished on'",
        timeout=300,
    )
    print("=== final btrfs replace status ===")
    print(machine.succeed("btrfs replace status -1 /mnt/storage"))

# --- Phase 6: Assert the inhibitor is released after the replace ---
#
# logind releases the inhibitor when the systemd-inhibit child exits, which
# happens when the SleepInhibitor RAII guard in cmd_replace is dropped. Use
# wait_until_succeeds to allow a brief settle window — the release is
# typically immediate but should not be timing-fragile.

with subtest("braid sleep inhibitor is released after replace"):
    def no_braid_inhibitor():
        return find_braid_sleep_inhibitor(list_inhibitors()) is None

    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        if no_braid_inhibitor():
            break
        time.sleep(0.5)
    else:
        post = list_inhibitors()
        raise AssertionError(
            f"braid sleep inhibitor still held 30s after replace finished: {post}"
        )

    post = list_inhibitors()
    print(f"=== inhibitors post-replace: {post} ===")

# --- Phase 7: Pool integrity ---

with subtest("Pool integrity after replace"):
    machine.succeed("mountpoint -q /mnt/storage")
    post_sha = machine.succeed("sha256sum /mnt/storage/payload").split()[0]
    assert post_sha == payload_sha, (
        f"payload sha256 changed across replace: pre={payload_sha} post={post_sha}"
    )
    fs_show = machine.succeed("btrfs filesystem show /mnt/storage")
    print("=== final btrfs filesystem show ===")
    print(fs_show)
    assert "MISSING" not in fs_show, (
        "phantom MISSING device after a clean (non-interrupted) replace — "
        "either the replace was interrupted or topology drifted:\n" + fs_show
    )
    assert "Total devices 3" in fs_show, (
        f"expected 3 devices after replace, got:\n{fs_show}"
    )
    # disk2 (the replace source) must be gone, disk4 (the target) must be present.
    assert "braid-disk4" in fs_show, f"replace target disk4 missing:\n{fs_show}"
    assert "braid-disk2" not in fs_show, f"replace source disk2 still present:\n{fs_show}"

machine.shutdown()
