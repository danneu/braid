# Plan: `braid idle` covers the full kernel exclusive-op set

## Context

`braid idle` is the gate autosuspend uses to decide whether to suspend the
NAS. Today it queries three btrfs status subcommands -- `scrub`, `balance`,
`replace` -- and treats anything else as idle (`cli/src/idle.rs:50-102`).

The kernel's exclusive-operation set is broader than that. From
`reference/btrfs-progs/common/utils.c:1188-1197` the values written to
`/sys/fs/btrfs/<fsid>/exclusive_operation` are:
`none`, `balance`, `balance paused`, `device add`, `device remove`,
`device replace`, `resize`, `swap activate`.

`btrfs balance status` only surfaces `balance` / `balance paused` (it uses
`BTRFS_IOC_BALANCE_PROGRESS`, see `reference/btrfs-progs/cmds/balance.c:879`).
A `device add`, `device remove`, `resize`, or `swap activate` --
regardless of whether started by braid or by an operator running `btrfs
...` directly -- prints `"No balance found"` and `idle.rs` reports the
pool as idle. Autosuspend then suspends the box mid-operation.

`SleepInhibitor` (`cli/src/inhibit.rs:108`) papers over this for
braid-initiated commands by holding a logind sleep inhibitor for the
lifetime of the operation, but an out-of-band `btrfs device remove`
(which on an HDD pool can run for hours) bypasses that defense.

`docs/decisions/016-auto-suspend.md:23` also understates the contract,
saying "btrfs exclusive operations (scrub, balance, replace)".

`cli/src/preflight.rs:60-188` already reads
`/sys/fs/btrfs/{fsid}/exclusive_operation` for mutating commands and
parses every kernel value. The fix wires the same sysfs read into
`cmd_idle` by *calling* `check_no_exclusive_op` directly -- not
duplicating the read/parse.

Note on scrub: scrub is **not** in the kernel exclop set. Sysfs cannot
detect a running scrub, so `parse_btrfs_scrub_status` must remain as
the sole scrub detector (and as the source of percentage for the busy
reason).

---

## Approach

### 1. Reuse, don't reimplement

In `cli/src/preflight.rs`, change visibility of `ExclusiveOp` (line 66),
`ExclusiveOpError` (line 112), and `check_no_exclusive_op` (line 176)
from private to `pub(crate)`. No logic change. The
preflight-policy-specific items (`ExclusiveOpPolicy`,
`check_exclusive_op_with_policy`) stay private.

### 2. Extend `IdleError` to cover the new failure modes

In `cli/src/idle.rs:42-48`, add `From` conversions / variants for both
new error sources reachable from `cmd_idle`:

```rust
#[derive(Debug, thiserror::Error)]
pub enum IdleError {
    #[error("command error: {0}")]
    Cmd(#[from] CmdError),
    #[error("parse error: {0}")]
    Parse(#[from] ParseError),
    #[error("probe error: {0}")]
    Probe(#[from] crate::probe::ProbeError),
    #[error("exclusive-op check error: {0}")]
    Exclop(String),  // wraps preflight::ExclusiveOpError's non-Busy variants
}
```

`ExclusiveOpError::Busy(op)` is **not** an error in `idle.rs` -- it is
the success signal for "pool is busy", so we map it to `BusyReason`
inside `cmd_idle` (see step 3) and only the `Read(..)` /
`Unrecognized(..)` variants become `IdleError::Exclop`. (Wrapping as
`String` keeps `idle.rs` from depending on `preflight::ExclusiveOpError`
internals while still surfacing the message to autosuspend's stderr.)

### 3. Rewrite `cmd_idle`

New signature:

```rust
pub fn cmd_idle<R: CommandRunner, F: Filesystem>(
    runner: &R,
    fs: &F,
    mount_point: &MountPoint,
) -> Result<IdleResult, IdleError>
```

Body:

1. `is_btrfs_mounted(runner, mount_point)?` -- unchanged. Return
   `PoolOffline` if false.
2. **Scrub probe first** (kept because scrub is not in the exclop set):
   call `BtrfsScrubStatus`, parse with `parse_btrfs_scrub_status`. If
   running, return `BusyReason::ScrubRunning { pct }`.
3. `let fsid = probe_fsid(runner, mount_point)?;` --
   `cli/src/probe.rs:354`. The `?` works once `IdleError` has the
   `From<ProbeError>` from step 2.
4. `match preflight::check_no_exclusive_op(fs, &fsid)` -- the same
   helper preflight uses. `Ok(())` falls through to `IdleResult::Idle`.
   `Err(ExclusiveOpError::Busy(op))` maps `op` to a `BusyReason`
   variant. `Err(ExclusiveOpError::Read(..) |
   ExclusiveOpError::Unrecognized(..))` returns
   `IdleError::Exclop(e.to_string())` (fail-closed -- exit 2).

Drop the `BtrfsBalanceStatus` and `BtrfsReplaceStatus` arms entirely,
including their parser calls. The parsers stay in the codebase for
TUI / `braid status` use.

### 4. Extend `BusyReason`

`cli/src/idle.rs:18-24`:

- Keep `ScrubRunning { pct: Option<u8> }` (subprocess-derived).
- Replace `BalanceRunning { pct_left }` and `BalancePaused { pct_left
  }` with payload-less `Balance` and `BalancePaused`. Sysfs gives no
  percentage; `braid idle` is exit-code-driven so the human stdout
  line is informational.
- Replace `ReplaceRunning { pct: f64 }` with payload-less
  `DeviceReplace`.
- Add `DeviceAdd`, `DeviceRemove`, `Resize`, `SwapActivate`.
- Update the `Display` impl to print short, plain strings such as
  `"device remove in progress"`, `"resize in progress"`. Use `--`
  not em-dashes (CLAUDE.md "CLI Output Style").

### 5. Wire the `Filesystem` parameter at the call site

`cli/src/main.rs:539-565`: construct `RealFilesystem` next to
`RealRunner` (one extra line) and pass `&fs` to `cmd_idle`. Update the
clap doc-comment at `cli/src/main.rs:44` from `"no scrub/balance/
replace"` to a concise phrase that does not enumerate the full list,
e.g. `"no scrub or btrfs exclusive operation"`.

### 6. Test rewrite (targeted at the new behavior)

Replace the existing `idle.rs` tests (currently lines 124-end). Use
the `MockFs` + `MockRunner` pattern from `cli/src/lock.rs:592-701`,
but with **exact-path** matching, not suffix matching:

- The test fixture seeds a known `BtrfsFilesystemShow` mock that
  returns a specific fsid (e.g.
  `12345678-1234-1234-1234-123456789abc`).
- `MockFs::with_exclop` records the *exact* expected path
  `/sys/fs/btrfs/12345678-1234-1234-1234-123456789abc/exclusive_operation`
  and its `read_to_string` returns the configured body only when
  asked for that path. Any other path returns `NotFound`. This
  guarantees the test fails if `cmd_idle` reads the wrong fsid (or
  the wrong file), which is the whole point of the new behavior.
- Helper to seed the `FindmntJson` + `BtrfsFilesystemShow` mocks
  needed by `probe_fsid`.

Cases covered (axis 1: changes the plan makes; axis 2: claims about
existing behavior):

- `PoolOffline`, all-quiet idle, scrub running (still subprocess-
  detected), each non-`none` exclop value (`balance`, `balance
  paused`, `device add`, `device remove`, `device replace`,
  `resize`, `swap activate`).
- Unrecognized exclop value -> `IdleError::Exclop`.
- Sysfs read error (e.g. `MockFs` returns `NotFound`) ->
  `IdleError::Exclop`.
- Scrub-running short-circuit: assert that `probe_fsid`'s mocks are
  *not* required (proving sysfs read is not attempted).
- Negative: a `MockRunner` seeded *without*
  `BtrfsBalanceStatus` / `BtrfsReplaceStatus` mocks must not panic
  with `MissingMock`, proving those subprocess paths are gone.

### 7. VM test update (replace coverage)

`tests/cli/replace-inhibits-suspend.py:120-156` (Phase 3a) currently
asserts `"replace running"` in `braid idle` output and times the call
to pin that `BtrfsReplaceStatus` uses `-1`.

After the refactor, `braid idle` no longer calls `BtrfsReplaceStatus`
at all, so:

- Update the substring assertion from `"replace running"` to the new
  Display string (e.g. `"device replace"`).
- Keep the 5-second timeout assertion -- a sysfs read is even faster
  than the previous subprocess probe, so this remains a valid
  promptness check, but it no longer protects the `BtrfsReplaceStatus
  -1` contract. Update the subtest's intent comment to reflect that.
- Move the `BtrfsReplaceStatus -1` contract to a Rust unit test in
  `cli/src/cmd.rs` (or wherever `CmdRequest::BtrfsReplaceStatus` is
  rendered to argv) -- a single assert that the rendered command
  contains `-1`. This keeps the original incident protection without
  routing through `braid idle`.

### 8. Documentation sweep

All of the following currently enumerate `(scrub, balance, replace)`
or describe `braid idle` as parsing those three btrfs status commands.
Update them to describe the new behavior: scrub via `btrfs scrub
status`, all other exclusive operations via
`/sys/fs/btrfs/<fsid>/exclusive_operation`.

| File | Lines / item |
|------|--------------|
| `cli/src/main.rs` | line 44 clap doc-comment for `Idle,` |
| `manual/commands/idle.md` | line 9 (autosuspend description), 29 (exit code 1 description), 35-38 (busy output examples), 59-61 (numbered behavior list) |
| `manual/guides/power-management.md` | line 21 (table row "btrfs exclusive operations: scrub, balance, replace") |
| `manual/guides/nixos-configuration.md` | line 131 (`braid idle` -- scrub, balance, or replace in progress) |
| `FEATURES.md` | line 30 (`**idle** -- check if pool is idle (no scrub/balance/replace)`) |
| `docs/decisions/016-auto-suspend.md` | line 23 (the `(scrub, balance, replace)` claim) and line 25-27 (the bullet that says "braid already has robust parsers for all btrfs status commands" -- now partly via sysfs, not parsers) |

---

## Critical files

| File | What changes |
|------|--------------|
| `cli/src/idle.rs` | Rewrite `cmd_idle`, extend `BusyReason` and `IdleError`, replace tests |
| `cli/src/preflight.rs` | `pub(crate)` on `ExclusiveOp`, `ExclusiveOpError`, `check_no_exclusive_op` |
| `cli/src/main.rs` | Pass `&RealFilesystem` to `cmd_idle`; update clap doc-comment line 44 |
| `cli/src/cmd.rs` (or argv-rendering test home) | New unit test pinning `BtrfsReplaceStatus` argv contains `-1` |
| `tests/cli/replace-inhibits-suspend.py` | Update Phase 3a assertion + intent comment |
| `manual/commands/idle.md` | Behavior + busy-string examples |
| `manual/guides/power-management.md` | Check-source table row |
| `manual/guides/nixos-configuration.md` | Idle bullet under autosuspend checks |
| `FEATURES.md` | Idle command bullet |
| `docs/decisions/016-auto-suspend.md` | Decision wording |

## Reused helpers (no new code, no duplication)

- `probe_fsid(runner, mount_point)` -- `cli/src/probe.rs:354`
- `Filesystem` trait + `RealFilesystem` -- `cli/src/probe.rs:13-51`
- `preflight::check_no_exclusive_op` -- `cli/src/preflight.rs:176`
  (called directly; not reimplemented)

## Out of scope

- Replace-progress and balance-progress percentages in
  `BusyReason::Display`. Sysfs does not provide them; subprocess
  probes for them are exactly what we are removing.
- Moving `ExclusiveOp` from `preflight.rs` to `probe.rs`. Tempting
  given probe.rs already owns `Filesystem` and `probe_fsid`, but a
  pure refactor the bug fix does not require.

---

## Verification

1. `just test-rust` -- exercises new `idle.rs` unit tests and the new
   `BtrfsReplaceStatus -1` argv pin.
2. `cargo build -p braid-cli` -- catches the `cmd_idle` signature
   change at the `main.rs` call site.
3. `just test-vm replace-inhibits-suspend` -- confirms the updated
   Phase 3a assertion still passes against a live in-flight replace.
4. `just test-vm` -- runs the full VM suite; any other test that calls
   `braid idle` will surface broken assumptions.
5. Manual: on a dev VM, run `btrfs balance start -dusage=0 /mnt/...`,
   then `braid idle; echo $?` -- expect exit 1 and stdout describing a
   balance. Repeat with a `btrfs device remove` (where the previous
   behavior incorrectly reported idle) and confirm exit 1 with
   "device remove" in the output.
