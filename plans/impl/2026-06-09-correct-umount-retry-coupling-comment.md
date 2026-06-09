# Plan: correct the systemd-stop umount-retry timeout-coupling comment

## Context

A review finding (Testing/Low) claimed the `SYSTEMD_STOP_UMOUNT_RETRY_ATTEMPTS = 60`
budget has an unguarded coupling to `braid-online.service` `TimeoutStopSec` and asked
for a Rust test pinning `60 * UMOUNT_RETRY_DELAY` below that timeout.

A verify-issue pass found the finding **misframed**, and traced the misframe to one
misleading sentence in the code itself. The comment above the constant
(`cli/src/lock.rs#SYSTEMD_STOP_UMOUNT_RETRY_ATTEMPTS`) ends with "Stay below
braid-online TimeoutStopSec", which implies a tight upper-bound coupling. The
authoritative design says otherwise:

- `cli/src/main.rs#run_systemd_stop_lock` shows `--deadline-secs`
  (`lockSystemdStopDeadlineSecs`, default 270) bounds **only** stop-coordinator +
  pool-lock acquisition. The 60-attempt umount retry runs **after** that, inside
  `cmd_lock_systemd_stop`, with no sub-timeout.
- ADR 018 `docs/design/decisions/018-systemd-lifecycle.md#execstop-bounded-wait-pattern`
  states this directly: the deadline "bounds only stop-coordinator and pool-lock
  acquisition; once lock cleanup reaches ... `umount`, any kernel wait to quiesce
  btrfs has no userspace timeout and is bounded only by the unit's `TimeoutStopSec`."
  The 60-attempt count is a **lower-bound** heuristic (outlast the transient
  `BTRFS_IOC_BALANCE_V2` mount-fd hold after the parent dies during shutdown); the
  upper bound is systemd's SIGKILL backstop, which the design accepts (btrfs RAID1 is
  crash-consistent, so SIGKILL mid-umount degrades to a recoverable unclean unmount).
- The genuine coupling the finding reached for -- `lockSystemdStopDeadlineSecs <
  braidOnlineStopTimeoutSecs` -- is the **deadline**, not the umount budget, and is
  **already** guarded by eval tests (`tests/eval/lock-systemd-stop-deadline-assertion.nix`
  and `.../lock-systemd-stop-deadline-assertion-fails.nix`, which read both Nix
  constants symbolically).

The proposed test would therefore guard nothing real: a literal `== 60` pin fixes a
fuzzy physical heuristic to a magic number, and an "umount budget < TimeoutStopSec"
assertion would hardcode the Nix `braidOnlineStopTimeoutSecs` into Rust -- a fresh
unguarded cross-language coupling, strictly worse than the status quo.

**Intended outcome:** the comment stops inviting this misreading. A future reviewer
reads the constant's real intent (lower-bound, SIGKILL-backstopped) and does not file
the same finding. No behavioral change; no test added.

## Change

Single file: `cli/src/lock.rs`. Rewrite the two-line `//` comment above
`SYSTEMD_STOP_UMOUNT_RETRY_ATTEMPTS` (currently lines 19-20) into a `///` doc comment
that (a) keeps the accurate lower-bound rationale, (b) replaces "Stay below
braid-online TimeoutStopSec" with the accurate backstop model, and (c) cites ADR 018.

`///` (not `//`) matches the prevailing style for semantically significant private
consts in this crate (e.g. `cli/src/doctor.rs#METADATA_CHUNK_HEADROOM`,
`cli/src/confirm.rs#CONFIRM_MAX_BYTES`) and binds the doc unambiguously to the const.

Before:

```rust
const UMOUNT_RETRY_ATTEMPTS: u32 = 3;
// During shutdown, the Rust mutator can die before its blocking btrfs-progs
// balance child releases the mount fd. Stay below braid-online TimeoutStopSec.
const SYSTEMD_STOP_UMOUNT_RETRY_ATTEMPTS: u32 = 60;
```

After (proposed wording; finalize at implementation):

```rust
const UMOUNT_RETRY_ATTEMPTS: u32 = 3;
/// Longer transient-busy umount retry for the systemd-stop path. During
/// shutdown the Rust mutator can die before its blocking btrfs-progs balance
/// child releases the mount fd, so this count must outlast that transient
/// hold. It is a lower-bound heuristic, not an upper-bound coupling: it is not
/// gated by `--deadline-secs` (which bounds only lock acquisition) and need
/// not fit under `TimeoutStopSec` -- post-lock cleanup is backstopped by
/// systemd's SIGKILL, not by this count. See ADR 018
/// `docs/design/decisions/018-systemd-lifecycle.md#execstop-bounded-wait-pattern`.
const SYSTEMD_STOP_UMOUNT_RETRY_ATTEMPTS: u32 = 60;
```

Constraints:
- ASCII only (`--`, straight quotes); no Unicode substitutes.
- Cite by `path#heading-slug`, never a line number (per `docs/dev/doc-citations.md`).
  The anchor `#execstop-bounded-wait-pattern` resolves to the `### ExecStop
  bounded-wait pattern` heading in ADR 018 (confirmed to exist).

## Out of scope (deliberately)

- **No test.** Per the determination above and the user's docs-only choice. The
  retry-beyond-user-budget property is already covered by
  `cli/src/lock.rs#systemd_stop_retries_busy_umount_beyond_user_attempts`; the
  deadline/timeout coupling by the two eval tests; the (NOP-invisible) delay value by
  `umount_with_retry_sleeps_prod_delay_between_busy_attempts`.
- **No ADR edit.** ADR 018 already states the correct model; the comment is being
  brought into line with it, not vice versa.
- **No other doc/comment edits.** The other "stay below TimeoutStopSec" mentions
  (`docs/guides/nixos-configuration.md`, `modules/braid/storage.nix`) correctly
  describe the *deadline*, which genuinely must stay below -- they are accurate.
- **No sibling-const cleanup.** Leave `UMOUNT_RETRY_ATTEMPTS` / `UMOUNT_RETRY_DELAY`
  comments as-is; unrelated to this finding.

## Verification

1. `cargo build -p braid-cli` (or the crate's build recipe) -- a `///` placed directly
   above the const is a valid item doc comment; confirms placement.
2. `cargo clippy` -- stays warning-clean (project policy; converting `//` to `///`
   triggers no default-on lint since the comment is attached to an item).
3. `just test-rust` -- no behavioral change expected; confirms nothing regressed.
4. Grep that the misleading phrasing is gone and the citation is present:
   `rg "Stay below braid-online" cli/src` returns nothing;
   `rg "execstop-bounded-wait-pattern" cli/src/lock.rs` returns the new comment.
5. Sanity-check the anchor target: the heading `### ExecStop bounded-wait pattern`
   exists in `docs/design/decisions/018-systemd-lifecycle.md`.

## Commit

`docs(lock): correct systemd-stop umount-retry timeout coupling comment`

(Matches the established `docs(lock):` pattern for comment fixes in this file, e.g.
commit 2e1c7c6d.)
