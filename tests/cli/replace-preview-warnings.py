# Test: replace-preview-warnings
#
# See replace-preview-warnings.nix for scenario / intent / rationale.

import base64
import re
import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add_cmd(key):
    pq = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {pq} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 {key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes"
    )


def replace_cmd(old, new, extra=""):
    pq = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {pq} | "
        f"braid replace --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 --old {old} --new {new}=/dev/disk/by-id/virtio-{new} "
        f"--passphrase-stdin {extra}"
    )


# --- Setup: 2-disk RAID1 pool with keyfile enrollment ---

with subtest("Setup: build 2-disk pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))

with subtest("Setup: generate and enroll keyfile into both members"):
    # Mirrors braid-enroll.py's fixture. After this, the pool carries
    # keyfile enrollment in LUKS slot 1 on every member -- the
    # precondition for the keyfile-asymmetry PreviewNote to fire when
    # the replacement disk is PresentNotLuks and --enroll is omitted.
    machine.succeed("mkdir -p /tmp/kf")
    machine.succeed(
        "dd if=/dev/urandom of=/tmp/kf/braid.key bs=4096 count=1 iflag=fullblock"
    )
    machine.succeed("chmod 400 /tmp/kf/braid.key")
    pq = shlex.quote(passphrase)
    machine.succeed(
        f"printf '%s\\n' {pq} | braid enroll /tmp/kf --passphrase-stdin"
    )

# `braid` is wrapped with a tool PATH prefix. Resolve the unwrapped
# binary so /tmp/wrap/cryptsetup can win PATH lookup in probe-failure
# regression subtests.
braid_wrapped_path = machine.succeed("readlink -f $(command -v braid)").strip()
wrapper_source = machine.succeed(f"cat {braid_wrapped_path}")
m = re.search(r'(/nix/store/[^"\s]+/bin/braid)(?!\-)', wrapper_source)
assert m, f"could not locate unwrapped braid in wrapper:\n{wrapper_source}"
unwrapped_braid = m.group(1)
real_cryptsetup = machine.succeed("command -v cryptsetup").strip()


def install_cryptsetup_luksdump_wrapper(mode):
    """Install a cryptsetup shim.

    mode='all-fail': every luksDump fails.
    mode='fail-first': the first luksDump fails, later calls delegate.
    """
    if mode == "all-fail":
        wrapper_template = """#!/usr/bin/env bash
set -eu
if [ "${1:-}" = "luksDump" ]; then
    printf 'forced luksDump failure for replace test\\n' >&2
    exit 5
fi
exec __REAL_CRYPTSETUP__ "$@"
"""
    elif mode == "fail-first":
        wrapper_template = """#!/usr/bin/env bash
set -eu
if [ "${1:-}" = "luksDump" ]; then
    count_file=/tmp/wrap/luksdump-count
    if [ ! -e "$count_file" ]; then
        printf 1 > "$count_file"
        printf 'forced first luksDump failure for replace test\\n' >&2
        exit 5
    fi
fi
exec __REAL_CRYPTSETUP__ "$@"
"""
    else:
        raise AssertionError(f"unknown wrapper mode: {mode}")

    wrapper_script = wrapper_template.replace("__REAL_CRYPTSETUP__", real_cryptsetup)
    wrapper_b64 = base64.b64encode(wrapper_script.encode()).decode()
    machine.succeed(
        "rm -rf /tmp/wrap && mkdir -p /tmp/wrap && "
        f"printf '%s' {shlex.quote(wrapper_b64)} | base64 -d > /tmp/wrap/cryptsetup && "
        "chmod +x /tmp/wrap/cryptsetup"
    )


def replace_unwrapped_cmd(old, new, extra=""):
    pq = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {pq} | "
        f"PATH=/tmp/wrap:$PATH {unwrapped_braid} replace "
        f"--luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 "
        f"--luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 "
        f"--old {old} --new {new}=/dev/disk/by-id/virtio-{new} "
        f"--passphrase-stdin {extra}"
    )

# --- Phase 1: live-path dry-run ---

with subtest("Phase 1: live-path --dry-run prints preview on stdout, stderr empty"):
    # Intent: `braid replace --old disk1 --new disk3=... --dry-run` on a
    # healthy pool with keyfile enrollment and a fresh (non-LUKS)
    # replacement disk must emit the step preview on stdout and keep
    # stderr empty. This simultaneously pins two contracts:
    #   (a) dry-run is rendered via `ReplacePlan::preview().print()`
    #       to stdout only (PR 8 Preview migration);
    #   (b) keyfile-asymmetry is a PreviewNote::Warn on stdout, not a
    #       legacy confirmation-only `WARNING:` block on stderr.
    # Why it exists: dry-run should show interpretive replacement risks
    # in the single Preview stream while preserving empty stderr on
    # success.
    # Scenario: healthy 2-disk pool with keyfile enrolled; disk3 is
    # raw; operator previews the swap without passing `--enroll`.
    machine.succeed(
        f"{replace_cmd('disk1', 'disk3', '--dry-run')} "
        f">/tmp/live-out 2>/tmp/live-err"
    )
    out = machine.succeed("cat /tmp/live-out")
    err = machine.succeed("cat /tmp/live-err")

    assert "btrfs replace start" in out, (
        f"live-path dry-run stdout must contain `btrfs replace start`;"
        f" got: {out!r}"
    )
    assert "cryptsetup close braid-disk1" in out, (
        f"live-path dry-run stdout must contain the `cryptsetup close"
        f" braid-disk1` step for a Live source; got: {out!r}"
    )
    assert err == "", (
        f"live-path dry-run stderr must be empty on success; got: {err!r}"
    )
    assert "WARNING:" not in out, (
        f"live-path dry-run stdout must not use legacy WARNING prefix;"
        f" got: {out!r}"
    )
    assert "WARNING:" not in err, (
        f"live-path dry-run stderr must not use legacy WARNING prefix;"
        f" got: {err!r}"
    )
    assert "[warn] Existing pool drives have a keyfile (keyslot-1)" in out, (
        f"dry-run stdout must carry the keyfile-asymmetry warning;"
        f" got: {out!r}"
    )
    assert "Existing pool drives have a keyfile" not in err, (
        f"dry-run stderr must not carry the keyfile-asymmetry body;"
        f" got: {err!r}"
    )
    assert "\x1b[" not in out, (
        f"dry-run stdout must be plain without a TTY; got: {out!r}"
    )

with subtest("Phase 1b: failed keyfile probe becomes dry-run warning"):
    # Intent: when cryptsetup luksDump fails, replace --dry-run must emit
    # probe uncertainty as a PreviewNote::Warn on stdout and keep stderr
    # empty.
    # Why it exists: successful dry-run owns a single stdout Preview stream;
    # interpretive diagnostics belong there, not in stderr or a suppressed
    # side path.
    # Scenario: run the unwrapped braid binary with a cryptsetup shim that
    # fails every luksDump while previewing disk1 -> disk3.
    install_cryptsetup_luksdump_wrapper("all-fail")
    (status, _) = machine.execute(
        f"{replace_unwrapped_cmd('disk1', 'disk3', '--dry-run')} "
        f">/tmp/probe-dry-out 2>/tmp/probe-dry-err"
    )
    assert status == 0, f"dry-run should succeed despite luksDump shim; exit {status}"
    out = machine.succeed("cat /tmp/probe-dry-out")
    err = machine.succeed("cat /tmp/probe-dry-err")

    assert "btrfs replace start" in out, (
        f"dry-run stdout must still contain the replace preview; got: {out!r}"
    )
    assert "[warn] could not check keyfile enrollment" in out, (
        f"dry-run stdout must carry probe-failure warning notes; got: {out!r}"
    )
    assert "proceeding as if no keyfile is enrolled" in out, (
        f"dry-run stdout must carry the canonical probe-failure suffix; got: {out!r}"
    )
    assert err == "", (
        f"dry-run stderr must stay empty when luksDump is shimmed; got: {err!r}"
    )

# --- Phase 2: --yes real-run (live path) ---

with subtest("Phase 2: --yes real-run renders probe warning on stderr"):
    # Intent: `braid replace --yes` (no `--dry-run`) with failed
    # keyfile probes must render the same PreviewNote warning to stderr
    # before mutating.
    # Why it exists: real-run and dry-run should share note wording while
    # preserving stream ownership: stdout is for command output, stderr is
    # for real-run diagnostics.
    # Scenario: pool still has disk1 + disk2 with keyfile; disk3 is
    # raw; operator runs `braid replace --yes` without `--enroll`.
    # Pool after: disk2 + disk3 (disk1 replaced in-place).
    install_cryptsetup_luksdump_wrapper("all-fail")
    (status, _) = machine.execute(
        f"{replace_unwrapped_cmd('disk1', 'disk3', '--yes')} "
        f">/tmp/yes-out 2>/tmp/yes-err"
    )
    assert status == 0, f"--yes real-run should succeed despite luksDump shim; exit {status}"
    out = machine.succeed("cat /tmp/yes-out")
    err = machine.succeed("cat /tmp/yes-err")

    assert "WARNING:" not in out, (
        f"--yes real-run stdout must not use legacy WARNING prefix;"
        f" got: {out!r}"
    )
    assert "WARNING:" not in err, (
        f"--yes real-run stderr must not use legacy WARNING prefix;"
        f" got: {err!r}"
    )
    assert "Existing pool drives have a keyfile" not in out, (
        f"--yes real-run stdout must not carry keyfile-asymmetry body;"
        f" got: {out!r}"
    )
    assert "Existing pool drives have a keyfile" not in err, (
        f"--yes real-run stderr must not carry keyfile-asymmetry body;"
        f" got: {err!r}"
    )
    assert "could not check keyfile enrollment" not in out, (
        f"--yes real-run stdout must not carry probe-failure warning;"
        f" got: {out!r}"
    )
    assert "[warn] could not check keyfile enrollment" in err, (
        f"--yes real-run stderr must carry probe-failure warning;"
        f" got: {err!r}"
    )
    assert "proceeding as if no keyfile is enrolled" in err, (
        f"--yes real-run stderr must carry canonical probe-failure suffix;"
        f" got: {err!r}"
    )

    # Sanity: the replace actually happened.
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "/dev/mapper/braid-disk3" in fi_show, (
        f"--yes real-run should have swapped disk1 -> disk3; got:\n{fi_show}"
    )
    assert "/dev/mapper/braid-disk1" not in fi_show, (
        f"--yes real-run should have removed braid-disk1; got:\n{fi_show}"
    )

with subtest("Phase 2b: enroll replacement disk3 for interactive mixed-case probes"):
    # Phase 2 replaced disk1 with disk3 without --enroll. Enroll disk3
    # before the interactive no-mutation checks so a mixed probe
    # (first luksDump fails, second succeeds) can prove slot 1 is occupied.
    pq = shlex.quote(passphrase)
    machine.succeed(
        f"printf '%s\\n' {pq} | braid enroll /tmp/kf --passphrase-stdin"
    )

with subtest("Phase 2c: interactive failed probes print warning and make no mutation"):
    # Intent: failed keyfile probes are routed to stderr as canonical
    # PreviewNote warnings when no existing member proves slot 1 is
    # occupied.
    # Why it exists: interactive replace uses the same plan notes as
    # dry-run and --yes, so operators see one diagnostic style.
    # Scenario: every luksDump fails, then the operator answers `no`.
    install_cryptsetup_luksdump_wrapper("all-fail")
    (status, _) = machine.execute(
        "printf 'no\\n' | "
        "PATH=/tmp/wrap:$PATH "
        f"{unwrapped_braid} replace --old disk2 --new disk4=/dev/disk/by-id/virtio-disk4 "
        ">/tmp/int-fail-out 2>/tmp/int-fail-err"
    )
    assert status != 0, "interactive `no` should abort the replace"
    err = machine.succeed("cat /tmp/int-fail-err")
    assert "[warn] could not check keyfile enrollment" in err, (
        f"interactive stderr must contain the probe-failure warning; got: {err!r}"
    )
    assert "proceeding as if no keyfile is enrolled" in err, (
        f"interactive stderr must carry the canonical probe-failure suffix; got: {err!r}"
    )
    assert "Existing pool drives have a keyfile" not in err, (
        f"all-failure probe must not claim keyfile enrollment; got: {err!r}"
    )
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "/dev/mapper/braid-disk4" not in fi_show, (
        f"answering no must not mutate the pool; got:\n{fi_show}"
    )

with subtest("Phase 2d: interactive mixed failure plus enrollment suppresses probe warning"):
    # Intent: if one probe fails but another existing member proves slot 1
    # is occupied, replace prints the keyfile-asymmetry warning and
    # suppresses the redundant probe-failure warning.
    # Why it exists: the actionable recommendation is to pass --enroll;
    # uncertainty from a failed earlier probe should not add noise once
    # enrollment is known.
    # Scenario: first luksDump fails, the next luksDump delegates to real
    # cryptsetup and sees the enrolled slot, then the operator answers no.
    install_cryptsetup_luksdump_wrapper("fail-first")
    (status, _) = machine.execute(
        "printf 'no\\n' | "
        "PATH=/tmp/wrap:$PATH "
        f"{unwrapped_braid} replace --old disk2 --new disk4=/dev/disk/by-id/virtio-disk4 "
        ">/tmp/int-mixed-out 2>/tmp/int-mixed-err"
    )
    assert status != 0, "interactive `no` should abort the replace"
    err = machine.succeed("cat /tmp/int-mixed-err")
    assert "[warn] Existing pool drives have a keyfile (keyslot-1)" in err, (
        f"mixed probe stderr must contain keyfile-asymmetry warning; got: {err!r}"
    )
    assert "WARNING:" not in err, (
        f"mixed probe stderr must not use legacy WARNING prefix; got: {err!r}"
    )
    assert "could not check keyfile enrollment" not in err, (
        f"mixed probe stderr must suppress probe-failure note; got: {err!r}"
    )
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "/dev/mapper/braid-disk4" not in fi_show, (
        f"answering no must not mutate the pool; got:\n{fi_show}"
    )

# --- Phase 3: missing-path dry-run ---

with subtest("Phase 3: missing-path --dry-run prints preview on stdout, stderr empty"):
    # Intent: on a degraded pool (disk2 simulated dead), a
    # `braid replace --old disk2 --new disk4=... --dry-run` must emit
    # the missing-path step preview on stdout and keep stderr empty.
    # Why it exists: missing-path dry-run flows through the same
    # `Preview::render` seam as live. Leaking anything to stderr
    # (probe events, degraded banners, etc.) regresses the Preview
    # contract for a second code path. Today's replace-dead-disk VM
    # test asserts the mutation succeeded, not stream routing.
    # Scenario: pool {disk2, disk3} (post-Phase-2), simulate disk2
    # physical failure via cryptsetup close + degraded remount.
    # Operator previews rebuilding onto disk4.
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk2")
    machine.succeed("mount -o degraded /dev/mapper/braid-disk3 /mnt/storage")
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "missing" in fi_show.lower(), (
        f"Phase 3 requires degraded pool with missing device;"
        f" got:\n{fi_show}"
    )

    machine.succeed(
        f"{replace_cmd('disk2', 'disk4', '--dry-run')} "
        f">/tmp/miss-out 2>/tmp/miss-err"
    )
    out = machine.succeed("cat /tmp/miss-out")
    err = machine.succeed("cat /tmp/miss-err")

    assert "btrfs replace start" in out, (
        f"missing-path dry-run stdout must contain `btrfs replace start`;"
        f" got: {out!r}"
    )
    assert "cryptsetup close" not in out, (
        f"missing-path dry-run must not carry a cryptsetup close step"
        f" (no old mapper on the Missing source path); got: {out!r}"
    )
    assert err == "", (
        f"missing-path dry-run stderr must be empty on success;"
        f" got: {err!r}"
    )
    assert "WARNING:" not in out, (
        f"missing-path dry-run stdout must not leak WARNING; got: {out!r}"
    )
    assert "WARNING:" not in err, (
        f"missing-path dry-run stderr must not leak WARNING; got: {err!r}"
    )

machine.shutdown()
