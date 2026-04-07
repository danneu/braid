# Repro test: btrfs replace interrupted mid-flight

## Problem

There is no end-to-end test that observes what happens to a btrfs RAID1 pool when a `btrfs replace` operation is interrupted mid-flight on braid's pinned NixOS toolchain. Two adjacent scenarios are tested — `tests/cli/recover-replace-not-started.py` (crash before `btrfs replace start` runs) and `tests/cli/recover-replace-completed.py` (crash after replace completes) — but the in-flight crash window in between is uncovered.

That gap matters because braid's mount path explicitly does *not* try to resume an interrupted replace. The pinned upstream documentation in `reference/btrfs-progs/Documentation/btrfs-replace.rst:41-47` states that on v6.19+ kernels an interrupted device replace is canceled by the kernel, and the user must restart it from the beginning. Whether that cancellation actually surfaces cleanly through `btrfs replace status`, `btrfs filesystem show`, `braid status`, and `braid recover` on this stack is currently unknown. Without a test, we cannot tell whether the user is left with an obviously-recoverable pool, a silently-degraded pool, or something stranger; we also cannot tell whether `braid recover` fires at all in this state.

The unrelated balance leg of the same general question is already handled: `docs/principles.md:22` documents that braid mounts always include `skip_balance`, which is precisely why interrupted balances become *paused* and are caught by `emit_paused_balance_warning` (`cli/src/status.rs:728`). No work is needed there.

## Goal

Add one repro VM test that:

1. Starts a real `braid replace` on a 3-disk RAID1 pool with enough data to make the operation interruptible.
2. Crashes the VM while the replace is actively running.
3. After reboot, captures the post-crash state visible to the user (`btrfs replace status`, `btrfs filesystem show`, `braid status`, `braid recover`, journal files, pool.json, mapper status).
4. Asserts a small set of safety-floor invariants that must hold regardless of how btrfs handles the interruption.
5. Locks in the observed kernel-level outcome as concrete assertions, so future upstream drift fails the test loudly.

A short findings note records the observed behavior so any subsequent product change (e.g. new guidance in `braid status` or `braid recover`) can be designed against real data.

## Approach

The test uses braid's existing repro-test pattern (`.nix` wrapper + `.py` script + explicit `flake.nix` entry).

The test is built and tightened in two passes inside this task:

1. **First pass** — implement the test with the safety-floor assertions and a printed transcript of post-crash state. Run it once.
2. **Second pass** — using the transcript from the first run, edit the same `.py` to add concrete assertions on the observed kernel and braid behavior. The shipped test contains both the safety floor and the observation locks.

The reason for the two-pass shape is that the test has to discover what to assert before it can assert it; pinning behavior before observing it would prejudge the outcome.

## Files to create

| Path | Role |
|---|---|
| `tests/repro/btrfs-replace-interrupted-mid-flight.nix` | NixOS VM wrapper. Mirrors `tests/repro/btrfs-remove-enospc-crash.nix`. |
| `tests/repro/btrfs-replace-interrupted-mid-flight.py` | Test script. Header per AGENTS.md → Test Conventions (Intent / Why it exists / Scenario). Reuses `add_cmd` / `replace_cmd` helper shape from `tests/cli/recover-replace-completed.py:30-47`. |
| `flake.nix` | New `repro-btrfs-replace-interrupted-mid-flight` entry next to the other `repro-btrfs-replace-*` checks at `flake.nix:390-394`. |
| `plans/wip/sharded-drifting-beaver-findings.md` | ~30-60 line findings note. Written between the two implementation passes; committed with the test. |

### `.nix` wrapper

```nix
{ braid }:
{
  name = "btrfs-replace-interrupted-mid-flight";

  nodes.machine = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk2"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk3"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk4"; }
    ];

    environment.systemPackages = [
      braid
      pkgs.cryptsetup
      pkgs.btrfs-progs
    ];

    environment.etc."braid/config.json".text = builtins.toJSON {
      mount_point = "/mnt/storage";
    };
  };

  testScript = builtins.readFile ./btrfs-replace-interrupted-mid-flight.py;
}
```

Four 1024 MiB disks: three pool members plus one replacement target. 1024 MiB gives the replace enough work to be interruptible without inflating runtime.

### `flake.nix` entry

Inserted next to `flake.nix:390-394`:

```nix
repro-btrfs-replace-interrupted-mid-flight = pkgs.testers.nixosTest (
  import ./tests/repro/btrfs-replace-interrupted-mid-flight.nix {
    braid = linuxCrane.braid;
  }
);
```

The `repro-` prefix is mandatory: `flake.nix:560-562` derives `reproChecks` from `checksFor` by filtering on `hasPrefix "repro-"`, so the prefix is what makes the check show up under `just test-repro`.

### `.py` test flow

1. **Bring up a 3-disk RAID1 pool.** `braid add disk1`, `disk2`, `disk3` via `printf passphrase | braid add ... --passphrase-stdin --yes`. Confirm `mountpoint -q /mnt/storage`.
2. **Write enough data to make replace measurable and verifiable.** `dd if=/dev/urandom of=/mnt/storage/payload bs=1M count=400`, then `sync`. Compute and store `sha256sum /mnt/storage/payload` for later integrity verification.
3. **Capture pre-crash state into the transcript.** `print(machine.succeed("uname -r"))`, `print(machine.succeed("btrfs filesystem show /mnt/storage"))`, `print(machine.succeed("cat /var/lib/braid/pool.json"))`.
4. **Start the replace asynchronously.** Background the `braid replace --old disk2 --new disk4=/dev/disk/by-id/virtio-disk4 --passphrase-stdin --yes` call (`machine.execute(..., check_return=False, timeout=...)` or shell `&`). Poll `btrfs replace status /mnt/storage` in a `for _ in range(60): ...` loop until reported progress is non-zero. Hard-fail the test if no in-flight progress is observed within the budget — that would mean the scenario didn't actually exercise an interrupt, so the test is invalid rather than the product is broken.
5. **Crash the VM mid-replace.** `machine.crash()` — strongest available involuntary interruption, closer to power-loss than a signal or freeze.
6. **Reboot and capture post-remount state.** `machine.start()`, `machine.wait_for_unit("multi-user.target")`. Then capture and `print` each of the following so the entire post-crash picture is in the test log:
    - `cryptsetup status braid-disk1` (and disk2/3/4 as applicable).
    - `braid unlock --passphrase-stdin` — capture stdout, stderr, exit code via `machine.execute`.
    - `btrfs filesystem show /mnt/storage`.
    - `btrfs replace status /mnt/storage`.
    - `braid status`.
    - `cat /var/lib/braid/pool.json`.
    - `test -f /var/lib/braid/pending-op.json && cat /var/lib/braid/pending-op.json || echo NO_JOURNAL`.
    - If a journal exists: `braid recover --passphrase-stdin`. Capture stdout/stderr/exit code.
    - Final `btrfs filesystem show /mnt/storage` and `braid status`.

### Safety-floor assertions

These four asserts ship in the first commit and never become observation-dependent. They must hold regardless of what btrfs decides to do.

- `braid unlock` exit code is either 0 or a structured non-zero. Stderr must not contain `panicked at` or `RUST_BACKTRACE` — no Rust panic, no stack trace.
- After the test sequence completes, `mountpoint -q /mnt/storage` succeeds.
- `sha256sum /mnt/storage/payload` matches the value captured in step 2 — no silent data loss.
- At least one of `disk2` (replace source) or `disk4` (replace target) is visible in `btrfs filesystem show /mnt/storage`. The pool must not silently drop both sides of the replace.

### Observation-lock assertions (added in the second pass)

After the first run produces the transcript, edit the same `.py` to convert the observed kernel-level outcome into hard assertions. Examples — actual text and direction depend on what the run reveals:

- `assert "no operation running" in machine.succeed("btrfs replace status /mnt/storage")` if that's what the kernel actually reports post-cancel.
- An assertion on which devid survived in `btrfs filesystem show` (source or target).
- An assertion on whether a journal file exists post-crash.
- If `braid status` already says something useful about the canceled replace, an assertion on its text. If it doesn't, *do not* fabricate one — record the gap in the findings note instead, so a follow-up plan can address it.

Drop the corresponding `print(...)` lines once a fact becomes an assertion. Keep prints only for diagnostic context that isn't worth pinning.

### Findings note

`plans/wip/sharded-drifting-beaver-findings.md`. Target ~30-60 lines:

- Kernel version observed (`uname -r`) on the pinned stack.
- Excerpts of the post-crash transcript.
- One paragraph in plain language describing what actually happens to a btrfs replace interrupted mid-flight on this stack.
- Open questions surfaced by the run (e.g. "`braid status` does not mention the canceled replace").
- Explicit recommendation line: "no product change recommended" or "follow-up plan should add X to `braid status` / `braid recover`".

## Verification

1. Implement the `.nix`, the `.py` (with safety-floor asserts and transcript prints), and the `flake.nix` entry. Run:
   ```
   just test-repro repro-btrfs-replace-interrupted-mid-flight
   ```
   The argument is the full flake attribute name including the `repro-` prefix. The test should run to completion. Only the four safety-floor asserts are allowed to hard-fail.
2. Write `plans/wip/sharded-drifting-beaver-findings.md` from the captured transcript.
3. Edit the `.py` to add the observation-lock assertions, then re-run:
   ```
   just test-repro repro-btrfs-replace-interrupted-mid-flight
   ```
   Must pass.
4. Smoke-check the rest of the suite did not regress: `just test-rust`, then `just test-repro` (no test name) to run all repro checks together.
5. Eyeball: `tests/repro/btrfs-replace-interrupted-mid-flight.py` has the Intent / Why it exists / Scenario header per AGENTS.md → Test Conventions, and the `.nix` wrapper structurally matches `tests/repro/btrfs-remove-enospc-crash.nix`.

## Decision criteria for follow-up work

Driven by the findings note. There are two independent axes of follow-up — they should be sized separately.

### Path A — kernel-resume-on-mount (this test, kernel-version-independent)

The unclean-kill scenario this test exercises produces a broken `DEGRADED` topology after `braid recover`, with a phantom `MISSING` device left over from the kernel's resume-but-don't-swap behavior. This is in `fs/btrfs/dev-replace.c` and is **not** affected by any v6.19 work. The test will continue to lock in this behavior across kernel bumps.

Possible outcomes for the follow-up plan:

- **Status / recovery guidance.** `braid recover` already detects `recovered != pre && recovered != target` and prints a `note:`. The follow-up should escalate that to a hard error with a concrete cleanup recipe (likely involving `btrfs device remove missing`), and consider whether to refuse to clear `pending-op.json` until the topology matches one of the expected memberships.
- **braid bug exposed.** If the follow-up investigation discovers recovery panics, pool.json corruption, or anything beyond the documented degraded-topology state, triage as a bug with its own plan.

### Path B — v6.19 freeze/signal cancellation (separate, when kernel ≥6.19 reaches NixOS stable)

The v6.19+ freeze/signal cancellation work the upstream `btrfs-replace.rst` doc warns about is **orthogonal** to the path this test exercises. An unclean kill bypasses Path B entirely; this test will not flip when 6.19 lands. When kernel ≥6.19 reaches NixOS stable, the right step is to **add a sibling repro test** that drives one of the Path B triggers — most realistically a systemd cgroup freeze (which is what fires around `systemctl suspend`, and braid enables autosuspend by default per `docs/principles.md` §11) or `fsfreeze -f /mnt/storage`.

Sequencing for the Path B work:

1. **First decide** whether `braid replace` should be inhibiting suspend for the duration of the operation. The upstream doc explicitly recommends this. If yes, that is the actual user-visible fix, and the Path B repro test is a regression-prevention safety net that exists primarily to confirm the inhibitor works.
2. **Otherwise** (or in addition), design the "your replace was canceled, restart it" surfacing in `braid status` / `braid recover` and add the sibling repro test alongside it.

The Path B sibling test sketch lives in `plans/wip/sharded-drifting-beaver-findings.md` under "Recommendation → Path B". It is **not urgent** unless real users are hitting suspend-during-replace, but it should not be skipped when 6.19 lands — without it, NixOS stable users on autosuspend hosts have an unobserved cancellation path.

This task does not commit to any of those outcomes. Its deliverable is the test plus the findings note.
