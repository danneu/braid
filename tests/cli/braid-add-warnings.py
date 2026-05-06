# Test: braid add warning-routing fixtures (missing-devices).
#
# See braid-add-warnings.nix for scenario / intent / rationale.

import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add_cmd(key, extra=""):
    pq = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {pq} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 {key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes {extra}"
    )


# --- Phase 0: build a 2-disk RAID1 pool ---

with subtest("Setup: build 2-disk RAID1 pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    df = machine.succeed("btrfs fi df /mnt/storage")
    assert "Data, RAID1" in df, f"Expected RAID1, got:\n{df}"

# --- Phase 1: synthesize a missing device ---

with subtest("Kill disk2: unmount, close mapper, remount degraded"):
    # Same fixture pattern used by replace-dead-disk.py. btrfs sees
    # disk2 as MISSING; probe_pool reports missing_count=1.
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk2")
    machine.succeed("mount -o degraded /dev/mapper/braid-disk1 /mnt/storage")
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "missing" in fi_show.lower(), f"Expected missing device:\n{fi_show}"

# --- Phase 2: dry-run -> stdout [warn] line, stderr empty ---

with subtest("Dry-run missing-device warning routes to stdout as [warn]"):
    # Intent: `braid add disk3 --dry-run` on a pool with a missing
    # device must surface the missing-devices diagnostic on stdout via
    # `[warn] pool has 1 missing device...`, with stderr empty.
    # Why it exists: PR 7 migrated the legacy stderr eprintln! to a
    # `PreviewNote::Warn` body. A regression that left `warning:` baked
    # into the body would stack as `[warn] warning: pool has ...`;
    # a regression that routed the note back to stderr would exit 0
    # and still slip past add-lifecycle coverage.
    # Scenario: 2-disk pool with 1 missing, operator starts planning an
    # add of disk3.
    machine.succeed(
        f"{add_cmd('disk3', '--dry-run')} >/tmp/md-stdout 2>/tmp/md-stderr"
    )
    out = machine.succeed("cat /tmp/md-stdout")
    err = machine.succeed("cat /tmp/md-stderr")

    assert "[warn] pool has 1 missing device" in out, (
        "dry-run stdout must contain the `[warn] pool has ...` body-only line;"
        " got: {!r}".format(out)
    )
    assert "warning:" not in out, (
        "dry-run note body must not carry the legacy `warning:` prefix; got: {!r}".format(out)
    )
    assert "\x1b[" not in out, (
        "dry-run stdout must be plain without a TTY; got: {!r}".format(out)
    )
    assert err == "", (
        "dry-run stderr must be empty on success; got: {!r}".format(err)
    )

# --- Phase 3: real-run -> stderr has exact legacy warning line ---

with subtest("Real-run missing-device warning renders as canonical [warn]"):
    # Intent: `braid add disk3` (no --dry-run) with a missing device
    # must print the canonical `[warn] pool has 1 missing device.
    # Consider repairing with `braid replace --missing-id <devid>`
    # first. Use `braid status` to see device IDs.` line on stderr --
    # the SAME bytes dry-run produces on stdout. The add-local
    # `warning: ` legacy replay was removed; plan-derived Warn notes
    # now route through the shared `preview::render_notes_for_stderr`
    # in both dry-run and real-run.
    # Why it exists: guards against a regression that reintroduces
    # the legacy `warning:` prefix on the Ok real-run path, producing
    # two different wordings for the same note across modes.
    # Scenario: same as Phase 2 but without --dry-run. The add may
    # proceed or fail depending on btrfs's degraded-mount tolerance;
    # the test only asserts the warning wiring, not the downstream
    # outcome. Use execute so a downstream btrfs error does not
    # abort the test before we inspect stderr.
    ec = machine.execute(
        f"{add_cmd('disk3')} >/tmp/rmd-stdout 2>/tmp/rmd-stderr"
    )[0]
    err = machine.succeed("cat /tmp/rmd-stderr")

    expected_line = (
        "[warn] pool has 1 missing device. Consider repairing with"
        " `braid replace --missing-id <devid>` first. Use `braid status`"
        " to see device IDs."
    )
    assert expected_line in err, (
        "real-run stderr must contain the canonical `[warn] ...` line;"
        " exit={} stderr={!r}".format(ec, err)
    )
    assert "warning: pool has" not in err, (
        "real-run stderr must NOT carry the legacy `warning:` prefix;"
        " exit={} stderr={!r}".format(ec, err)
    )
    assert "\x1b[" not in err, (
        "real-run stderr must be plain without a TTY; exit={} stderr={!r}".format(ec, err)
    )

# --- Phase 4: preserved-context failure uses canonical [warn] on stderr ---
#
# Intent: when `plan_add` accumulates the missing-devices warn and then
# fails later inside add work-plan rendering (BraidLabeledNoBtrfs
# identity), stderr must show the canonical `[warn] pool has ...`
# line BEFORE the refusal error -- the SAME bytes the Ok path
# (`AddPlan::execute`) emits on real-run stderr and `Preview::render`
# emits on dry-run stdout.
#
# Why it exists: both `cmd_add`'s Err branch and `AddPlan::execute` now
# pipe their notes through the shared `preview::render_notes_for_stderr`
# helper. If a future change re-introduces a command-local legacy
# replay on either path, the two paths will diverge. This test pins
# the unified wording at the CLI boundary.
#
# Scenario: pool still has disk2 MISSING (from Phase 1). Craft disk4
# as a braid-labeled LUKS with zeroed btrfs signature so identity
# classification returns BraidLabeledNoBtrfs. Run `braid add disk4`
# (no --dry-run).

with subtest("Phase 4: preserved-context failure renders canonical [warn]"):
    dev4 = "/dev/disk/by-id/virtio-disk4"
    # LUKS-format disk4 with the braid-<name> label, then open the
    # mapper so plan_add's probe_config_disk sees PresentLuks with
    # mapper_open. No btrfs superblock inside -- that is the ambiguous
    # identity that add work-plan rendering rejects via
    # BraidLabeledNoBtrfs.
    machine.succeed(
        f"echo -n '{passphrase}' | cryptsetup luksFormat --batch-mode --key-file=- "
        f"--label braid-disk4 {luks_opts} {dev4}"
    )
    machine.succeed(
        f"echo -n '{passphrase}' | cryptsetup open --key-file=- {dev4} braid-disk4"
    )
    # Sanity: btrfs filesystem show on the mapper should NOT find a fs.
    ec_sanity = machine.execute(
        "btrfs filesystem show /dev/mapper/braid-disk4 2>&1"
    )[0]
    assert ec_sanity != 0, (
        "braid-disk4 must have no btrfs superblock for the "
        "BraidLabeledNoBtrfs branch to fire"
    )

    ec, _ = machine.execute(
        f"{add_cmd('disk4')} >/tmp/pc-stdout 2>/tmp/pc-stderr"
    )
    out = machine.succeed("cat /tmp/pc-stdout")
    err = machine.succeed("cat /tmp/pc-stderr")

    assert ec != 0, (
        "preserved-context add must fail (BraidLabeledNoBtrfs); "
        "exit={} stdout={!r} stderr={!r}".format(ec, out, err)
    )
    assert out == "", (
        "stdout must be empty on failure; got: {!r}".format(out)
    )
    warn_line = (
        "[warn] pool has 1 missing device. Consider repairing with"
        " `braid replace --missing-id <devid>` first. Use `braid status`"
        " to see device IDs."
    )
    warn_pos = err.find(warn_line)
    assert warn_pos != -1, (
        "stderr must carry the canonical `[warn] ...` line on the refusal "
        "path; got: {!r}".format(err)
    )
    # Identity-error wording comes from identity_to_error's
    # BraidLabeledNoBtrfs branch in cli/src/add.rs.
    err_pos = err.find("contains no btrfs superblock")
    assert err_pos != -1, (
        "stderr must carry the BraidLabeledNoBtrfs identity error; "
        "got: {!r}".format(err)
    )
    assert warn_pos < err_pos, (
        "warning must render BEFORE the error on the preserved-context "
        "stderr path; got: {!r}".format(err)
    )
    # Regression guard: the legacy `warning:` prefix must NOT appear --
    # plan-derived Warn notes now share one rendering across modes.
    assert "warning: pool has" not in err, (
        "Err-path must NOT emit the legacy `warning: pool has ...` form on "
        "stderr; got: {!r}".format(err)
    )

machine.shutdown()
