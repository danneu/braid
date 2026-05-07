# Surface actionable hint text on `btrfs replace start` scrub-collision rejection

## Context

`pool_replace_device` (`cli/src/pool.rs:279-304`) wraps
`btrfs replace start --enqueue -r -f -B`. When the kernel rejects the
call because a scrub is currently running on the pool, today the user
sees the raw upstream stderr with no recovery guidance:

```
ERROR: pool: btrfs replace failed (exit 1): ERROR: ioctl(DEV_REPLACE_START) on '/mnt/storage' returns error: scrub is in progress
```

Scrub is not part of the kernel's `exclusive_operation` set, so the
`--enqueue` flag braid passes (`cli/src/cmd.rs:629`) cannot wait it
out -- the kernel `BTRFS_IOCTL_DEV_REPLACE_RESULT_SCRUB_INPROGRESS`
result short-circuits the start, and upstream's
`replace_dev_result2string` (`reference/btrfs-progs/cmds/replace.c:50-64`)
emits the literal `"scrub is in progress"` substring in the START
ioctl error formatter (`:330-356`). A collision with
`braid-scrub.service` (which runs scrub on a monthly timer) or a
manual `btrfs scrub start` is the realistic case where this fires.

The fix mirrors the precedent established by `balance_error`
(`cli/src/pool.rs:64-81`, introduced in commit `48249c2` "detect ENOSPC
during balance and suggest dusage=0 recovery"), which classifies ENOSPC
stderr from `btrfs balance` and appends a one-line `hint: ...` recovery
suggestion. Outcome: an operator who hits the scrub collision learns
the exact next command to run without searching docs.

The plan deliberately scopes the classifier to the scrub-collision
path only, after evidence-based pivot from a broader two-rejection
scope (see `## Out of scope` below for the `already started` rationale).

## Files to modify

- `cli/src/pool.rs` -- add helper, wire into `pool_replace_device`, add
  unit tests that drive `pool_replace_device` through `MockRunner`.
- `tests/repro/btrfs-replace-rejected-during-scrub.nix` (new) +
  `tests/repro/btrfs-replace-rejected-during-scrub.py` (new) --
  live-tool behavior-lock that pins the upstream stderr wording for
  the scrub-collision path.
- `flake.nix` -- register the new repro under
  `repro-btrfs-replace-rejected-during-scrub`, alongside the existing
  `repro-btrfs-replace-*` entries near `flake.nix:495-505`.

`pool_replace_device` is called only from `cli/src/replace.rs:586`; the
recover path waits via `wait_for_kernel_replace_to_finish`
(`cli/src/recover.rs:2777`) instead of re-invoking
`pool_replace_device`, so the new hint surfaces only at user-invoke
time of `braid replace`.

## Implementation

### 1. Add `replace_error` helper

Insert next to `balance_error` (`cli/src/pool.rs:64-81`). Same shape
as `balance_error` (which also classifies a single substring -- "no
space left"):

- `let stderr = result.stderr.to_lowercase();` for case-insensitive
  substring matching.
- One classified branch, returning `PoolError::Failed` with the base
  message followed by `\nhint: ...` and the mount point inline:
  - `stderr.contains("scrub is in progress")` ->
    ``hint: a scrub is currently running -- check progress with `braid status`, or run `btrfs scrub cancel {mount_point}` to abort it before retrying``
- Fall-through: plain `"btrfs replace failed (exit {N}): {stderr}"`
  (byte-identical to the current message, so unrelated failures are
  unaffected).

Keep the helper module-private (`fn replace_error(...) -> PoolError`);
only one caller, no need for `pub(crate)`. The single-branch shape
parallels `balance_error` and leaves room for additional substrings
later if a new realistically-reachable rejection path emerges.

### 2. Wire into `pool_replace_device`

`cli/src/pool.rs:296-302`: replace the inline
`PoolError::Failed(format!(...))` with `replace_error(mount_point, &result)`.

### 3. Unit tests (call-site behavior boundary)

Add two tests in the existing `tests` module in `cli/src/pool.rs`,
using the `// Intent / Why / Scenario` preamble convention and the
existing `mp()` helper. **Drive `pool_replace_device` through
`MockRunner`** (mirroring `pool_replace_device_propagates_failure` at
`cli/src/pool.rs:1132-1160`), not the private `replace_error` helper
directly -- the goal is to lock the user-visible behavior at the
function boundary so a wiring regression (helper added but
`pool_replace_device` left calling the inline format) fails the test.

- **`pool_replace_device_scrub_in_progress_includes_hint`**
  Mock `BtrfsReplaceStart` -> exit 1 with stderr `"ERROR: ioctl(DEV_REPLACE_START) failed on \"/mnt/storage\": Operation not permitted, scrub is in progress"`.
  Call `pool_replace_device(...)`. Assert returned error contains
  `"hint:"`, `"scrub"`, and `"/mnt/storage"`.
- **`pool_replace_device_no_hint_for_unrelated_failure`**
  Mock `BtrfsReplaceStart` -> exit 1 with stderr `"target device is too small"`.
  Call `pool_replace_device(...)`. Assert returned error contains
  `"target device is too small"` AND does NOT contain `"hint:"`.

The existing
`pool_replace_device_propagates_failure`
(`cli/src/pool.rs:1132-1160`) continues to pin basic non-zero
propagation; no change needed there. The new
`_no_hint_for_unrelated_failure` test is the one that locks "no
spurious hint on unrelated stderr".

### 4. Live-tool behavior-lock repro test

Per the project pattern documented in `docs/testing.md:64-72`, any
classifier of the form `stderr.contains("<wording>")` against an
external tool requires a registered repro/VM test that asserts the
same wording directly against live tool output. `just test-parsers`
does NOT cover `btrfs replace start` rejection stderr, so a new repro
is required.

Add `tests/repro/btrfs-replace-rejected-during-scrub.{nix,py}`
modelled on `tests/repro/cryptsetup-close-mounted.{nix,py}` and
`tests/repro/btrfs-replace-rejects-smaller-target.{nix,py}` (3-disk
LUKS+btrfs RAID1 setup so the kernel actually accepts a real replace).
The script uses one subtest:

- **`scrub is in progress` (poll-until-running)**
  A naive "scrub start; immediately replace" approach is
  timing-dependent: scrub on small VM disks can finish in under a
  second. The deterministic approach is to poll scrub status until
  the kernel reports running, then immediately fire replace.

  Steps:
  1. Write a payload large enough that scrub does not finish
     instantly (target a single payload file in the multi-hundred-MiB
     range across both 512 MiB RAID1 mirrors; sync afterwards). If
     scrub still finishes faster than the poll loop on the test host,
     fall back to repeating the payload write or raising the disk
     sizes in the test's `.nix` until scrub stays running for >2 s.
  2. Start scrub in the background: `btrfs scrub start /mnt/storage`.
  3. Poll `btrfs scrub status /mnt/storage` until output contains
     `Status:` followed by `running` (literal wording from
     `reference/btrfs-progs/cmds/scrub.c:340-343`). Hard-fail if not
     observed within (say) 10 s -- a flaky timing window must surface
     as a failure, not a silent skip.
  4. Invoke replace using the **exact braid argv shape**
     (`cli/src/cmd.rs:620-643`): `btrfs replace start --enqueue -r -f
     -B <devid> /dev/mapper/disk3 /mnt/storage 2>&1`. The `-B`
     (do_not_background) flag is load-bearing: the START-ioctl
     stderr formatter that emits `"scrub is in progress"` is gated
     by `if (do_not_background)` at
     `reference/btrfs-progs/cmds/replace.c:330-356`; without it,
     `daemon(0, 0)` detaches before the START ioctl runs and the
     shell never sees the error wording the classifier consumes.
     Capture exit code and stderr.
  5. Assert exit non-zero AND stderr (case-insensitive) contains
     `"scrub is in progress"`.
  6. `btrfs scrub cancel /mnt/storage` before shutdown.

Register in `flake.nix` near the existing `repro-btrfs-replace-*`
entries (`flake.nix:495-505`):

```nix
repro-btrfs-replace-rejected-during-scrub = pkgs.testers.nixosTest (
  import ./tests/repro/btrfs-replace-rejected-during-scrub.nix
);
```

A wording shift in the scrub-collision rejection path on a future
`nixpkgs` bump fails this test loudly, surfacing the drift before the
unit-level classifier silently misclassifies in production.

## Reuse / Existing utilities

- `balance_error` (`cli/src/pool.rs:64-81`) -- design template for the
  helper.
- `pool_replace_device_propagates_failure`
  (`cli/src/pool.rs:1132-1160`) -- reference shape for the new
  `MockRunner`-driven unit tests (same `with_output(BtrfsReplaceStart
  {...}, RawCommandOutput {...})` setup).
- `mp()` test helper (`cli/src/pool.rs::tests` -- returns
  `MountPoint("/mnt/storage".into())`) -- reused in both unit
  tests.
- `tests/repro/cryptsetup-close-mounted.{nix,py}` -- canonical
  behavior-lock-style repro referenced by `docs/testing.md:72`.
- `tests/repro/btrfs-replace-rejects-smaller-target.{nix,py}` -- 3-disk
  LUKS+btrfs RAID1 VM setup template; the new repro reuses the same
  disk layout and LUKS setup steps.
- `flake.nix:495-505` registration block -- new repro entry slots in
  alongside the existing `repro-btrfs-replace-*` lines.
- Test preamble convention (`// Intent / Why / Scenario`) -- mandated
  by `AGENTS.md` and consistently applied in both `pool.rs::tests`
  and `tests/repro/*.py`.

## Verification

Both gates are required; neither alone is sufficient (`docs/testing.md:64-72`).

- **Call-site behavior gate:** `just test-rust` -- runs the two new
  unit tests through `pool_replace_device` + `MockRunner`, plus the
  existing `pool_replace_device_propagates_failure`. A wiring
  regression (helper present but call site not updated) fails here.
- **Live-tool behavior-lock gate:**
  `just test-repro repro-btrfs-replace-rejected-during-scrub` --
  exercises a real kernel + `btrfs-progs` and asserts the
  scrub-collision stderr contains `"scrub is in progress"`. A
  `nixpkgs`-bump-induced wording shift fails here, before the
  classifier can silently misclassify in production. Also picked up
  by `just test-all`.
- The literal substring asserted in unit tests comes straight from
  upstream (`reference/btrfs-progs/cmds/replace.c:50-64` for
  `replace_dev_result2string`, `:330-356` for the START-ioctl error
  formatter). Substring matching in the helper is case-insensitive
  (`to_lowercase()`), so minor upstream wording shifts (e.g. `Scrub
  Is In Progress`) would still classify correctly; the live-tool
  gate is the authority on whether the substring is still present at
  all.

## Out of scope

- **Classifying the `"already started"` rejection.** Considered and
  dropped after evidence-based pivot. Three layers make this path
  effectively unreachable from `braid replace` in production:
  (1) `cmd_replace` always passes `--enqueue` (`cli/src/cmd.rs:629`),
  so any in-flight replace causes `check_running_fs_exclop` to wait
  rather than emit `"already started"`
  (`reference/btrfs-progs/common/utils.c:1278`).
  (2) The pending-op preflight (`cli/src/replace.rs:877` ->
  `cli/src/preflight.rs:42-55`) blocks re-invoke when
  `pending-op.json` is present, covering the killed-mid-replace
  case. (3) The kernel's own
  `BTRFS_IOCTL_DEV_REPLACE_RESULT_ALREADY_STARTED` is gated behind
  `ASSERT(0)` (`reference/linux/fs/btrfs/dev-replace.c:650-655`),
  so it is not a normal failure mode. The only theoretical residual
  is a microsecond-wide mount-resume race where the exclop sysfs
  flag has not yet flipped; the right surface for that case is
  `cmd_recover` (which already runs
  `wait_for_kernel_replace_to_finish`,
  `cli/src/recover.rs:2777`), not a hint at `cmd_replace` time.
  Adding a classifier (and the masked-sysfs repro required to lock
  it) would cost real test complexity for behavior braid does not
  meaningfully reach.
- Pre-empting the rejection at preflight time (e.g. probing scrub state
  before invoking replace) -- a heavier change, not justified by the
  small post-hoc UX gap this fix closes.
- Re-routing the operator to `braid recover` -- preflight already
  refuses re-invocation when `pending-op.json` exists
  (`cli/src/preflight.rs:42-55`), so by the time `pool_replace_device`
  runs there is no journal to recover from. The hint correctly points
  at the kernel-level cleanup command (`btrfs scrub cancel`).
- Changing `device_remove_result` (`cli/src/pool.rs:267-276`) -- the
  finding cited those line numbers but they belong to a different
  function; `device remove` has its own kernel-rejection surface and
  is out of scope for this plan.
