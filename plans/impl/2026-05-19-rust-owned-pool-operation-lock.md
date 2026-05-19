# Move pool-operation locking from wrapper into Rust

## Context

`modules/braid/braid-wrapper.sh` is currently the single source of truth
for "which braid subcommands acquire `/run/braid-pool.lock`." It contains
its own clap-shaped subcommand parser (lines 9-22) and a case statement
(lines 52-79) that lists the locked commands. Today's list:
`unlock | add | recover | remove | remove-missing | replace | discover`
(fail-fast `-n`), `ack` (`-w 10`), `monitor` (`-n`, silent exit 0). This
list has drifted in lockstep with `cli/src/main.rs` over the last six
weeks across commits `1abc903`, `3ee1674`, `ac219e4`, and `905e9ca` --
each one a separate "add command X to the wrapper's flock case" patch.

Two real gaps remain and one structural cause:

1. **`braid lock` is not in the flock case.** When an in-flight mutator
   (`add`, `replace`, `remove`, `remove-missing`, `recover`) holds the
   pool lock, a concurrent user-initiated `braid lock` runs straight
   through to `cmd_lock`'s `umount` while the mutator is mid-btrfs work.
   The mutator's next syscall fails on the freshly-unmounted FS, its
   journal is already written, and the user is forced into `braid
   recover` even though the only fault was the concurrent `braid lock`.
   On shutdown the failure mode is worse: `braid-online.service`'s
   `ExecStop = braid lock` races the mutator under a fixed 5-minute
   `TimeoutStopSec`, and `systemctl poweroff` can leave the disks in an
   inconsistent topology.

2. **`braid enroll` (the LUKS slot-1 keyfile enrollment command) is
   also not in the flock case.** Same race vector for keyslot
   operations.

3. **Locking lives outside the binary.** The "pool mutators are
   serialized" rule is invisible from inside the Rust CLI. The wrapper
   must mirror `cli/src/main.rs`'s subcommand set, the post-success
   lifecycle work, and the contention message verbatim. Every new
   mutator forces a wrapper edit; missing one is undetectable from a
   Rust-side test.

`docs/decisions/018-systemd-lifecycle.md` lines 150-154 prescribe the
shape of fix 1: "give the ExecStop path its own internal entry point
with a deadline below `TimeoutStopSec` (e.g. a hidden `braid lock
--systemd-stop` flag that waits on the lock up to a deadline)." That
prescription is unimplemented; the current `ExecStop=` uses plain
`braid lock` with a `BRAID_SYSTEMD_EXECSTOP=1` env var that only
governs the wrapper's post-success `systemctl stop` reentry, not lock
acquisition.

The goal of this pivot is to make Rust the single owner of
pool-operation locking. The wrapper shrinks to PATH setup + exec. The
contention rule, the locked-command list, the post-success lifecycle
work, the contention message, and the ExecStop bounded-wait variant all
live in one place, visible from one read site.

## Goal (end state)

- `modules/braid/braid-wrapper.sh` is three lines: shebang,
  `export PATH=...`, `exec @braidBin@ "$@"`. No subcommand parsing, no
  `flock`, no `9>&-`, no `systemctl` calls.
- Every pool mutator (`unlock`, `add`, `recover`, `remove`,
  `remove-missing`, `replace`, `enroll`, `lock`, `discover --write`)
  and every alert-state mutator (`ack`, `monitor`) acquires
  `/run/braid-pool.lock` from `cli/src/main.rs` dispatch BEFORE any
  config / membership / journal / probe / prompt I/O. Read-only
  commands (`status`, `doctor`, `idle`, `tui`, `ups`, `help`, bare
  `discover`) do not acquire.
- `braid lock` has two modes:
  - Plain `braid lock` (user-initiated): acquires the stop
    coordinator and the pool lock non-blocking; runs `cmd_lock`;
    writes `done\n` to the coordinator; then synchronously stops
    `braid-online.service`; releases both locks.
  - Hidden `braid lock --systemd-stop --deadline-secs <N>` (invoked
    only by `braid-online.service` ExecStop): probes the stop
    coordinator; if a plain `braid lock` holds it, polls the
    coordinator content for `done\n` (and the flock for release)
    within `N` seconds, then exits 0 on `done\n`, runs `cmd_lock`
    on coordinator release after a crash, or returns
    `PoolLockError::DeadlineExpired` on timeout.
- A new stop-coordinator flock at
  `/run/braid-stop-coordinator.lock` orders the two modes of
  `braid lock` so that an external `systemctl stop` racing a slow
  plain `braid lock` cannot deadlock the unit until
  `TimeoutStopSec`. See the "Design" section.
- `braid-online.service`'s `ExecStop=` uses
  `braid lock --systemd-stop --deadline-secs ${cfg.lockSystemdStopDeadlineSecs}`.
  No more `BRAID_SYSTEMD_EXECSTOP=1` env var.
- A new NixOS module option `braid.lockSystemdStopDeadlineSecs`
  (default 270) is type-checked with an eval-time assertion that it is
  strictly less than the unit's `TimeoutStopSec`. The timeout is
  single-sourced via a shared `braidOnlineStopTimeoutSecs`
  constant (see "NixOS module option + assertion" below); both
  the assertion predicate and the unit's `TimeoutStopSec=` read
  from it so the literal `300` lives in exactly one place.
- The post-success lifecycle work that currently lives in the wrapper
  (mountpoint check, `chown root:<storageGroup>`, `chmod 2770`,
  `systemctl start braid-online.service`, post-lock synchronous
  `systemctl stop braid-online.service`) moves into Rust, inside the
  held-lock critical section, with a state-enum snapshot rule
  (described below) on the `systemctl start` decision. The post-lock
  stop is synchronous (no `--no-block`) so plain `braid lock` returns
  only after `braid-online.service` is `inactive` -- matching today's
  wrapper invariant at decision 018:131 and avoiding the queued-stop
  race documented in the `mark_offline` section.
- `docs/principles.md` Principle 12 is updated to include `lock` and
  `enroll`. `docs/decisions/018-systemd-lifecycle.md` is updated to
  reflect the new Rust-side ownership. A new
  `docs/decisions/026-pool-lock-rust-owned.md` records the structural
  decision and supersedes the relevant prose in 018. The numbering
  picks the next free slot: `021-wait-in-unlock.md` through
  `025-browse-vs-curated.md` are already taken in
  `docs/decisions/`. `docs/index.md` gets a new entry for ADR 026 in
  the `decisions/` list so the file is discoverable.

## Design

### Module: `cli/src/pool_lock.rs` (new)

Mirrors the seam pattern in `cli/src/inhibit.rs`:

```rust
pub trait PoolLockGuard {}
impl<T> PoolLockGuard for T {}

pub trait AcquirePoolLock {
    /// Non-blocking acquire. Used by user-facing mutators (`unlock`,
    /// `add`, `recover`, `remove`, `remove-missing`, `replace`,
    /// `enroll`, `lock`, `discover --write`) and by `monitor`. On
    /// contention -> `PoolLockError::AlreadyHeld`.
    fn acquire(&self) -> Result<Box<dyn PoolLockGuard>, PoolLockError>;

    /// Polled bounded-wait acquire. Used by `ack` (10 s). On
    /// timeout -> `PoolLockError::AlreadyHeld` (same retry message as
    /// `acquire()`, since the user's recovery action is identical:
    /// retry once the active operation finishes).
    fn acquire_with_timeout(&self, timeout: Duration)
        -> Result<Box<dyn PoolLockGuard>, PoolLockError>;

    /// Polled bounded-wait acquire for `braid lock --systemd-stop
    /// --deadline-secs N`. `deadline` must be strictly less than the
    /// unit's `TimeoutStopSec` (enforced at NixOS eval time). On
    /// timeout -> `PoolLockError::DeadlineExpired { waited }` with a
    /// distinct message so the systemd-stop failure is greppable in
    /// the journal and not confused with ordinary user contention.
    fn acquire_with_systemd_stop_deadline(&self, deadline: Duration)
        -> Result<Box<dyn PoolLockGuard>, PoolLockError>;
}

pub struct RealPoolLock { path: PathBuf }
pub struct RealPoolLockGuard { fd: OwnedFd }

#[cfg(test)]
pub struct RecordingPoolLock { /* records acquire calls + mode */ }
```

Implementation notes:

- **Lock API: BSD `flock(2)` (per-open-file-description scope),
  not POSIX OFD locks.** Both the pool lock and the stop
  coordinator below use `nix::fcntl::flock` with `FlockArg::
  LockExclusiveNonblock`, which wraps the `flock(2)` system
  call -- a BSD-style advisory lock, not `fcntl(F_OFD_SETLK)`.
  The two APIs happen to share two semantics relevant here
  (the lock is scoped to a single open-file-description and is
  released on process death), but they are separate kernel
  interfaces with subtly different inheritance and downgrade
  rules; a future maintainer grepping for "OFD lock" in
  `flock(2)` / `fcntl(2)` man pages will land on the wrong API.
  Anywhere this plan or the resulting code talks about "flock"
  going forward refers to BSD `flock(2)`. Drop the standalone
  "OFD" abbreviation throughout the module doc.
- `RealPoolLock::acquire` opens `/run/braid-pool.lock` with
  `O_RDWR | O_CREAT | O_CLOEXEC`, mode `0600`, then `flock(fd,
  LockExclusiveNonblock)`. `EWOULDBLOCK` -> `PoolLockError::AlreadyHeld`.
  Wrap the `RawFd` -> `OwnedFd` conversion in a small helper so the
  fd is closed deterministically if `flock` fails.
- A second seam, `AcquireStopCoordinator` + `RealStopCoordinator`,
  lives in the same module (or a sibling file). The coordinator
  file is `/run/braid-stop-coordinator.lock`. It uses the same
  `O_RDWR | O_CREAT | O_CLOEXEC` open and the same BSD
  `flock(2)` primitives as the pool lock. Two responsibilities:

  1. **Single-stop-transition gate.** Only one stop transition can
     be in progress at a time. Plain `braid lock` and the
     `--systemd-stop` ExecStop reentry both acquire this lock
     BEFORE pool-lock acquisition. This deliberately reverses the
     order from the pool lock so that the reentry can know
     unambiguously whether a plain `braid lock` is "owning" the
     current stop transition just by probing the coordinator's
     flock state.
  2. **Cleanup-done flag.** The same file's CONTENT encodes
     whether plain `braid lock` has finished `cmd_lock` and is
     past the irreversible cleanup. Concretely: the coordinator
     holder writes the literal string `done\n` to the file via a
     single `write` after `cmd_lock` succeeds; readers
     `stat()` and `read()` the file to detect this. The flock
     and the file content live on the same fd, so both states
     vanish on coordinator drop (the next plain acquirer
     `ftruncate`s to 0 on entry as a defense-in-depth against
     stale content from a crashed predecessor).

  Two operations:

  - `acquire(&self) -> Result<StopCoordinatorGuard, ...>` --
    non-blocking exclusive flock. On `EWOULDBLOCK`, returns
    `Held`. Used by plain `braid lock` as its first step (so plain
    fails fast if another stop transition is in flight) and by the
    `--systemd-stop` ExecStop reentry as its first probe.
  - `poll_for_done_or_release(&self, deadline: Duration)
    -> StopCoordinatorPollResult` -- used ONLY by the
    `--systemd-stop` reentry when its initial `acquire()` returned
    `Held`. Polls with 100 ms ticks until either:
    1. The coordinator's file content equals `done\n` (the plain
       holder finished `cmd_lock` and is about to call its
       synchronous `systemctl stop`) -> returns `Done`.
    2. A non-blocking flock acquire on the coordinator succeeds
       (the plain holder crashed; kernel released the flock)
       -> returns `Acquired(StopCoordinatorGuard)` so the reentry
       can run its own `cmd_lock`.
    3. `deadline` elapses -> returns `Deadline`. The reentry then
       prints `PoolLockError::DeadlineExpired` and exits 1.

    Polling, not blocking on `flock`, is the deadlock-avoidance
    mechanism: plain `braid lock` holds the coordinator across its
    synchronous `systemctl stop`. A blocking `flock` on the
    coordinator from the reentry would wait for plain to release,
    plain would wait for `systemctl stop` to return, and
    `systemctl stop` would wait for the reentry's ExecStop to
    return -- a three-way deadlock until `TimeoutStopSec` fires.
    Polling lets the reentry observe the `done` content while the
    coordinator is still held and exit 0 immediately, returning
    `systemctl stop` cleanly.

  Crash safety: if plain crashes mid-`cmd_lock` (before writing
  `done`), the kernel releases the flock on process death.
  The reentry's next poll tick acquires the coordinator, reads
  empty content (no `done`), and runs full `cmd_lock` under the
  bounded-wait pool lock.
- `acquire_with_timeout` and `acquire_with_systemd_stop_deadline`
  both poll with `LockExclusiveNonblock` on a short interval
  (250 ms) until acquired or the timeout passes. They differ only in
  the error variant returned on timeout
  (`AlreadyHeld` vs `DeadlineExpired { waited }`). Polling avoids
  `SIGALRM`/`pthread_kill` complexity.
- `O_CLOEXEC` means every subsequent `Command::spawn` (notably
  `RealSleepInhibitor`'s `systemd-inhibit` fork) auto-closes the fd.
  This replaces the wrapper's `9>&-` invariant.
- `Drop` on `RealPoolLockGuard` closes the fd; the kernel releases
  the flock.
- `PoolLockError`:
  - `AlreadyHeld` -- `Display` emits the FULL wrapper-compatible line
    `"braid: another braid operation is already in progress (pool
    lock /run/braid-pool.lock is held); retry once it finishes"`.
    Asserted by a unit test so the line stays grep-stable across
    refactors.
  - `DeadlineExpired { waited: Duration }` -- distinct message:
    `"braid: pool lock not released within {waited:.0?}; aborting
    --systemd-stop"`.
  - `Io(io::Error)` -- any other open/flock failure.

`cli/Cargo.toml` gains `nix = { version = "<pin from nixpkgs>",
features = ["fs", "user"] }`. The `fs` feature provides
`nix::fcntl::{open, flock, OFlag, FlockArg}` and `nix::sys::stat::Mode`.
The `user` feature provides `nix::unistd::{chown, User, Group}` (used
by `online_state.rs::chown`).

### Module: `cli/src/online_state.rs` (new)

Encapsulates the wrapper's post-success lifecycle work behind one seam
so tests can fake the whole surface (chown/chmod cannot be observed
via `RecordingRunner` if implemented as direct syscalls).

```rust
pub enum UnitActiveState {
    Active, Activating, Deactivating, Inactive, Failed, Reloading,
    Refreshing, Unknown(String),
}

pub trait OnlineStateOps {
    fn unit_active_state(&self, unit: &str) -> Result<UnitActiveState, OnlineError>;
    fn is_mountpoint(&self, path: &Path) -> Result<bool, OnlineError>;
    fn chown(&self, path: &Path, owner: &str, group: &str) -> Result<(), OnlineError>;
    fn chmod(&self, path: &Path, mode: u32) -> Result<(), OnlineError>;
    fn systemctl_start(&self, unit: &str) -> Result<(), OnlineError>;
    fn systemctl_stop(&self, unit: &str, no_block: bool) -> Result<(), OnlineError>;
    /// Returns the unit names listed in `BoundBy=` on `unit`, parsed
    /// from `systemctl show -P BoundBy <unit>` via the
    /// `SystemctlShowBoundBy` `CmdRequest` variant.
    ///
    /// Failure mode split (deliberate, so callers can distinguish
    /// "couldn't run systemctl at all" from "systemctl ran and
    /// reported a nonzero exit"):
    /// - `Ok(Vec::new())` -- the call succeeded (exit 0) and the
    ///   `BoundBy` property was empty (whitespace-only stdout).
    /// - `Err(OnlineError::Spawn { source: CmdError })` -- the
    ///   subprocess failed to spawn or was killed by a signal;
    ///   mirrors the `Err(CmdError)` branch of
    ///   `CommandRunner::run` at `cli/src/cmd.rs:1151`.
    /// - `Err(OnlineError::SystemctlShow { exit_code, .. })` --
    ///   the subprocess ran to completion and exited nonzero;
    ///   mirrors the `Ok(RawCommandOutput { exit_status != 0 })`
    ///   branch of the same call.
    ///
    /// stdout is consumed as the `String` field of
    /// `RawCommandOutput` (`cli/src/cmd.rs:8`); the shared
    /// `output_to_raw` helper already decodes with
    /// `String::from_utf8_lossy` at `cli/src/cmd.rs:1199`, so this
    /// method never sees raw bytes and cannot reject on invalid
    /// UTF-8. Parsing is `str::split_whitespace`, which is robust
    /// against the lossy U+FFFD replacement.
    ///
    /// Callers decide whether to surface the `Err`. `cmd_lock`'s
    /// BoundBy pre-step swallows the `Err` silently (matches
    /// `2>/dev/null || true` on the wrapper's `systemctl show
    /// -P BoundBy` line at `modules/braid/braid-wrapper.sh:118`).
    /// Any other future caller is free to log or propagate.
    fn list_bound_by(&self, unit: &str) -> Result<Vec<String>, OnlineError>;
}

pub struct RealOnlineStateOps<'a> { runner: &'a dyn CommandRunner }
#[cfg(test)]
pub struct RecordingOnlineStateOps { /* records each call as typed enum */ }
```

`RealOnlineStateOps`:
- `unit_active_state` runs `systemctl show -P ActiveState <unit>` via
  the existing `CommandRunner` seam. **Do NOT use `systemctl
  is-active`** -- it conflates `Activating` and `Deactivating` with
  "not active," which is the deadlock vector (see "Snapshot rule"
  below).
- `is_mountpoint` reuses `CmdRequest::MountpointCheck`. Exit 0 ->
  `Ok(true)`, exit 1 -> `Ok(false)`, other -> `OnlineError::Mountpoint`.
- `chown` uses `nix::unistd::chown` (resolve user/group via
  `nix::unistd::User::from_name` / `Group::from_name`).
- `chmod` uses `std::fs::set_permissions`.
- `systemctl_start` and `systemctl_stop` shell out via
  `CommandRunner` using two new `CmdRequest` variants
  (`SystemctlStart { unit }`, `SystemctlStop { unit, no_block }`).
- `list_bound_by` shells out via `CommandRunner` using the new
  `SystemctlShowBoundBy { unit }` `CmdRequest` variant. Failure
  mapping mirrors the runner boundary at
  `cli/src/cmd.rs:1150-1157` (`CommandRunner::run` returns
  `Result<RawCommandOutput, CmdError>`):
  - `Err(CmdError::_)` (spawn failure or signal-killed process)
    maps to `Err(OnlineError::Spawn { source })`.
  - `Ok(RawCommandOutput { exit_status, .. })` with `exit_status
    != 0` maps to
    `Err(OnlineError::SystemctlShow { exit_code: exit_status, .. })`.
  - `Ok(output)` with `exit_status == 0` parses
    `output.stdout` (already a lossily-decoded `String` per
    `cli/src/cmd.rs:1199`) with `str::split_whitespace` and
    returns `Ok(Vec<String>)` (empty when the property is empty).
  The two `Err` variants are kept distinct so future callers can
  tell "couldn't run systemctl at all" from "systemctl ran and
  said no." The failure is NOT swallowed inside
  `RealOnlineStateOps`; swallowing is the caller's responsibility
  (`cmd_lock` does it for either variant; future callers can
  choose). There is intentionally no UTF-8-invalid branch: the
  shared `output_to_raw` helper decodes the subprocess bytes with
  `String::from_utf8_lossy` before this seam ever sees them, so
  invalid bytes arrive as U+FFFD replacements and parse silently
  -- adding a "non-UTF-8" branch here would be dead code against
  the current `CommandRunner` contract.

`CommandRunner::run` returns `Ok` regardless of exit code. Each method
explicitly checks `exit_status` and converts nonzero to a typed
`OnlineError`. Callers (`snapshot`, `mark_online`, `mark_offline`) log
WARNINGs and never propagate -- preserving today's wrapper semantics
where post-success fixups are best-effort.

Three public functions:

- `snapshot(ops) -> OnlineSnapshot` -- called BEFORE the mutation
  begins, AFTER the `PoolLockGuard` is acquired. Records
  `online_state: UnitActiveState` (the entry state of
  `braid-online.service`). Used ONLY by `mark_online`'s start gate.
  Infallible -- a `systemctl show` failure records
  `UnitActiveState::Unknown(reason)`, which `mark_online` treats as
  "skip the start" (the safe default).

- `mark_online(snap, cfg, ops) -> Result<(), OnlineError>` -- called
  by `unlock`, `add`, `recover` after a successful mount, while the
  lock is still held.
  1. `is_mountpoint(cfg.mount_point)` -- if not mounted, no-op.
  2. If `cfg.storage_group` is `Some(group)`: `chown(mount_point,
     "root", group)` then `chmod(mount_point, 0o2770)`. If `None`,
     skip BOTH (matches today's wrapper).
  3. Only if `snap.online_state` was `Inactive` or `Failed`:
     `systemctl_start("braid-online.service")`. Skip on `Active`,
     `Activating`, `Reloading`, `Refreshing`, `Deactivating`,
     `Unknown(_)`. `Refreshing` is a real systemd `ActiveState`
     (see `reference/systemd/src/basic/unit-def.c:112` and
     `reference/systemd/src/core/unit.h:64`, where it's grouped
     with `Active` / `Reloading` as an active-like state) used
     during `RefreshExtensions=` and mount-mutation transitions;
     a `Refreshing` snapshot means the unit is already up and
     mid-transition, so issuing a `start` would be a no-op at
     best and a queued-job race at worst. Treating it like
     `Reloading` (skip, no warning) matches the active-like
     classification and avoids the spurious "Unknown" warning
     that would otherwise fire on this branch.
  4. **`Unknown(reason)` after a confirmed mountpoint emits a
     visibility WARNING** (the snapshot itself is silent so the
     skip-the-start decision is recoverable but the operator is
     told). Exact text, byte-for-byte:
     `braid: WARNING: could not read braid-online.service ActiveState
     ({reason}) -- pool is mounted but shutdown may not lock
     automatically`. The trailing clause matches the existing
     wrapper-side warning at `modules/braid/braid-wrapper.sh:158`
     ("pool is mounted but shutdown may not lock automatically") so
     operators see a stable substring across the migration. Without
     this warning, a failed `systemctl show` leaves a mounted pool
     without the shutdown hook AND without surfacing the fact --
     the operator would only discover it on the next shutdown.
  5. Each step's failure is logged as WARNING to stderr but does not
     return Err.

- `mark_offline(mount_point, ops) -> Result<(), OnlineError>` --
  called by **plain** `braid lock` after a successful unmount, while
  the pool lock is still held. **No snapshot, no state gate.** Only
  invoked from the user-facing dispatch arm; the
  `braid lock --systemd-stop` arm never calls `mark_offline`
  (calling `systemctl stop braid-online.service` from inside its own
  ExecStop would deadlock waiting for itself).
  1. `is_mountpoint(mount_point)` -- if still mounted, no-op. On
     error, log a WARNING and continue.
  2. `systemctl_stop("braid-online.service", no_block=false)` --
     synchronous stop, warn-only. We deliberately do NOT use
     `--no-block`: a queued stop returns immediately, leaving a
     pending ExecStop that can fire AFTER a subsequent
     `braid unlock` mounts the pool, racing to lock the freshly
     mounted pool. Synchronous stop blocks until the unit is
     `inactive`, which (combined with the `--systemd-stop` reentry
     fast-path described below) guarantees `braid lock` returns only
     when `braid-online.service` is actually inactive.

  The synchronous stop is safe against deadlock because plain
  `braid lock` acquires a **stop coordinator** flock on
  `/run/braid-stop-coordinator.lock` BEFORE its pool-lock
  acquisition and writes `done\n` to the coordinator file after
  `cmd_lock` succeeds. The `--systemd-stop` ExecStop reentry
  probes the coordinator's flock state and content (polling
  every 100 ms, never blocking on the flock) and exits 0 the
  moment it observes `done\n`. See "ExecStop scenarios" under
  `braid lock` two-mode entry below for the full state machine.
  The call chain for plain `braid lock`:

  1. Plain `braid lock` acquires the stop coordinator (non-
     blocking; fail-fast if another stop transition is in flight).
  2. Plain `braid lock` acquires the pool lock (non-blocking;
     fail-fast on mutator contention).
  3. Plain `braid lock` runs `cmd_lock` (unmounts pool, closes
     LUKS, sweeps orphan mappers).
  4. Plain `braid lock` writes `done\n` to the coordinator file.
  5. Plain `braid lock` calls synchronous
     `systemctl stop braid-online.service` (still holding both
     the coordinator and the pool lock).
  6. The unit's ExecStop fires
     `braid lock --systemd-stop --deadline-secs N`. The reentry
     polls the coordinator. The first or second tick observes
     `done\n` and exits 0 without acquiring either lock.
  7. The unit transitions to `inactive`. systemd reports stop
     succeeded.
  8. Plain `braid lock`'s synchronous stop returns. Plain drops
     the coordinator and the pool lock, then exits.

  This matches today's wrapper synchrony invariant (decision 018
  line 131) -- "the command returns only after the lifecycle owner
  is inactive" -- without introducing a queued-stop race, the
  three-way deadlock identified during plan review (external
  `systemctl stop` racing a slow plain `braid lock` before plain
  holds a marker), OR the orphan-mapper hole that a mount-only
  fast-path would have opened against decision 018:181 (stale
  `braid-online` after out-of-band unmount).

### Snapshot rule (deadlock avoidance)

The asymmetry (`mark_online` uses snapshot; `mark_offline` does not)
is intentional. Failure scenario the snapshot prevents:

1. User runs `systemctl stop braid-online.service`. ExecStop is
   already running `braid lock --systemd-stop`, blocked waiting to
   acquire the pool lock. The unit is `Deactivating`.
2. Concurrently, user runs `braid add`. Dispatch acquires the pool
   lock first (ExecStop is waiting on it). `braid add` snapshots
   `online_state = Deactivating`.
3. `braid add` finishes its mutation. `mark_online`'s start gate sees
   `Deactivating` -> skip. (A boolean `is-active` snapshot would have
   returned "not active," recorded `false`, issued `systemctl start`,
   which would queue behind the still-pending stop -- deadlock.)
4. `braid add` releases the pool lock. ExecStop acquires within
   deadline, runs `cmd_lock`, stop completes cleanly.

`mark_offline` doesn't need a snapshot because the synchronous
`systemctl stop` is unconditional (we always want the unit inactive
after a user lock), and the ExecStop reentry's fast-path -- not a
state gate -- is what prevents the recursion deadlock.

### Dispatch wiring in `cli/src/main.rs`

The pool lock is the first real execution boundary. Acquire in
`main.rs` dispatch BEFORE config / membership / journal / probe loads,
BEFORE prompts. Per-command pattern:

```rust
Commands::Add(args) if !args.dry_run => {
    let _pool_guard = match pool_lock.acquire() {
        Ok(g) => g,
        Err(PoolLockError::AlreadyHeld) => {
            eprintln!("{e}");  // verbatim contention line from Display impl
            std::process::exit(1);
        }
        Err(PoolLockError::Io(e)) => return Err(e.into()),
        Err(PoolLockError::DeadlineExpired { .. }) => unreachable!(),
    };
    let online_ops = RealOnlineStateOps::new(&runner);
    let snap = online_state::snapshot(&online_ops);
    let cfg = load_config(...)?;
    cmd_add(args, cfg, runner, &inhibitor, &fs)?;
    online_state::mark_online(&snap, &cfg, &online_ops);
}
```

Rules:

- `--dry-run` arms do NOT acquire the lock.
- `--help` / `--version` intercepted by clap before dispatch.
- `Discover` arm acquires only when `args.write == true`. Bare
  `discover` (read-only scan) does NOT acquire and proceeds even
  when the lock is held -- it never writes `pool.json`. See
  "Tests to revise" below for the corresponding test change.
- `cmd_*` signatures take already-loaded config (loaded under the
  guard) rather than loading it themselves.
- The dispatch acquisition site is the single source of truth for the
  contention message: `eprintln!("{e}")` on `Display` of
  `PoolLockError::AlreadyHeld`. Do NOT route through `print_cli_error`
  (which would prefix `error:`) and do NOT manually prepend `braid:`
  (which would double the prefix).

**Acquisition mode per command** (pins existing wrapper-side
semantics now enforced in Rust dispatch; values are asserted by the
existing `pool-lock-*-contention.py` and `alert-state-lock.py`
tests):

| Command                          | Method                                              | On timeout / contention                                                                                              |
| -------------------------------- | --------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------- |
| `unlock`                         | `acquire`                                           | print `AlreadyHeld`, exit 1                                                                                            |
| `add`                            | `acquire`                                           | print `AlreadyHeld`, exit 1                                                                                            |
| `recover`                        | `acquire`                                           | print `AlreadyHeld`, exit 1                                                                                            |
| `remove`                         | `acquire`                                           | print `AlreadyHeld`, exit 1                                                                                            |
| `remove-missing`                 | `acquire`                                           | print `AlreadyHeld`, exit 1                                                                                            |
| `replace`                        | `acquire`                                           | print `AlreadyHeld`, exit 1                                                                                            |
| `enroll`                         | `acquire`                                           | print `AlreadyHeld`, exit 1                                                                                            |
| `discover` (with `--write`)      | `acquire`                                           | print `AlreadyHeld`, exit 1                                                                                            |
| `discover` (no `--write`)        | (no acquire)                                        | -- runs even when lock is held; never writes `pool.json`                                                               |
| `lock` (plain)                   | `acquire`                                           | print `AlreadyHeld`, exit 1                                                                                            |
| `lock` (`--systemd-stop`)        | `acquire_with_systemd_stop_deadline(deadline_secs)` | print `DeadlineExpired { waited }`, exit 1 (ExecStop reports failed)                                                   |
| `ack`                            | `acquire_with_timeout(Duration::from_secs(10))`     | print `AlreadyHeld`, exit 1 (`tests/module/alert-state-lock.py:239-243` pins the 9-14 s wait window and retry message) |
| `monitor`                        | `acquire`                                           | exit 0 silently (no message; missed timer cycle is harmless, exit 1 would spuriously start `braid-alert.service`)      |

**Intentional operator-visible wording change for `ack`.**
Today, `modules/braid/braid-wrapper.sh:66` prints
`braid: another braid operation is in progress (pool lock
/run/braid-pool.lock is held); retry once it finishes` on `ack`
timeout, while `braid-wrapper.sh:57` prints `braid: another braid
operation is **already** in progress (pool lock
/run/braid-pool.lock is held); retry once it finishes` for every
other mutator. The plan unifies both paths to the latter wording
by routing `ack`'s timeout through the shared
`PoolLockError::AlreadyHeld` `Display` (the single source of
truth for the contention line, see the
`already_held_display_is_wrapper_compatible_verbatim` unit
test). The behavior is unchanged (same exit code, same retry
guidance, same bounded-wait window); only the literal "already"
appears in `ack`'s timeout output where today it doesn't.
`tests/module/alert-state-lock.py:241` only checks the
substrings `"in progress"` and `"retry"`, so the existing test
suite continues to pass; the change is silent to grep-based
operator workflows. Document this in the user-facing changelog
when the migration commits.

All other commands (`status`, `doctor`, `idle`, `tui`, `ups`, `help`,
internal `scrub-cancel` / `scrub-needs-resume` /
`scrub-resume-or-start`) do not acquire the lock.

### `braid lock` two-mode entry

Extend `LockArgs` (`cli/src/main.rs:128-133`) with two hidden flags
that require each other:

```rust
#[derive(Debug, Args)]
struct LockArgs {
    /// Show what would be done without making changes
    #[arg(long)]
    dry_run: bool,

    /// Hidden: invoked from braid-online.service ExecStop with
    /// bounded-wait acquisition.
    #[arg(long, hide = true, requires = "deadline_secs")]
    systemd_stop: bool,

    /// Hidden: maximum seconds to wait for the pool lock when
    /// --systemd-stop is set. Required with --systemd-stop;
    /// rejected otherwise.
    #[arg(long, hide = true, requires = "systemd_stop",
          value_parser = clap::value_parser!(u64).range(1..))]
    deadline_secs: Option<u64>,
}
```

Dispatch order in the `Lock` arm:

1. If `args.dry_run`: render the lock plan, return. No lock
   acquisition (matches today's `cmd_lock` dry-run behavior).
2. Else if `args.systemd_stop` (ExecStop reentry path):
   1. **Stop-coordinator probe.** Try non-blocking acquire on
      `/run/braid-stop-coordinator.lock`.
      - **Acquired** -- no plain `braid lock` is in the middle of
        a stop transition. `ftruncate` the file to 0 (clear any
        stale `done` content from a crashed predecessor), then
        fall through to step 2.2 holding the coordinator guard.
      - **Held** (a plain `braid lock` is mid-transition):
        call `poll_for_done_or_release(deadline =
        Duration::from_secs(args.deadline_secs.unwrap()))`. The
        poll observes the coordinator's content and flock state
        every 100 ms:
        - **`Done`** -- plain finished `cmd_lock` and wrote
          `done\n`. Exit 0 immediately. plain's synchronous
          `systemctl stop` returns cleanly. This is the ONLY safe
          shortcut: it proves plain has already done the
          irreversible cleanup (including orphan-mapper sweep)
          under its pool guard.
        - **`Acquired(guard)`** -- plain crashed (kernel released
          the flock); reentry now holds the coordinator with
          empty content. Fall through to step 2.2 holding both
          guards.
        - **`Deadline`** -- print
          `PoolLockError::DeadlineExpired`'s `Display` and exit 1.
          The unit's ExecStop reports failed; systemd cannot help
          us further because the contending plain `braid lock`
          (or mutator) is outside the unit's process tree.
   2. (Reached only when the reentry now holds the coordinator
      guard.)
      `pool_lock.acquire_with_systemd_stop_deadline(Duration::from_secs(
      remaining_deadline))`. On deadline expiry, print
      `DeadlineExpired` and exit 1. (`remaining_deadline` is
      `args.deadline_secs - time_already_spent_polling` so the
      total wall-clock budget is honored.)
   3. Run `cmd_lock` -- including its mount probe, scrub-stop,
      BoundBy stop, optional umount, and orphan-mapper sweep --
      exactly as plain `braid lock` would. The mount-already-gone
      and pool-state-Unmounted variants of `cmd_lock` (`Snapshot::
      Unmounted` in `cli/src/lock.rs`) already produce the right
      behavior for an out-of-band-unmount case (no umount to do;
      still scan and close `braid-*` orphan mappers). This honors
      decision 018:181 -- "out-of-band mount or unmount bypasses
      the wrapper and can leave `braid-online` stale; `braid lock`
      handles already-unmounted pools gracefully."
   4. Do NOT call `mark_offline` -- we ARE the unit's ExecStop;
      calling `systemctl stop braid-online.service` would block
      waiting for ourselves to finish.
   5. Drop pool lock, drop coordinator guard.
3. Else (plain user-facing `braid lock`):
   1. **Acquire the stop coordinator FIRST**: non-blocking
      `flock` on `/run/braid-stop-coordinator.lock`. On `Held` ->
      print the canonical contention line (a concurrent stop
      transition is in flight) and exit 1. The kernel guarantees
      this acquisition is observable to any concurrently-firing
      ExecStop reentry, so the reentry's probe in step 2.1 will
      correctly see `Held` for the entire window plain holds the
      coordinator. `ftruncate` the file to 0 (defense-in-depth
      against stale `done` content).
   2. Acquire the pool lock (non-blocking). On contention ->
      drop the coordinator, print the canonical contention line,
      exit 1.
   3. Run `cmd_lock` (the pool lock is held; orphan-mapper
      cleanup runs under the guard). **If `cmd_lock` returns
      `Err(_)` (umount EBUSY, scrub-stop failure, BoundBy
      consumer-stop failure that propagated, orphan-mapper close
      failure, etc.):**
      - Do NOT write `done\n` to the coordinator. The content
        marker is a TRUTH claim ("plain finished the
        irreversible cleanup") and lying to a concurrent ExecStop
        reentry would let it exit 0 against a still-mounted pool
        with open mappers.
      - Do NOT call `mark_offline`. Running `systemctl stop
        braid-online.service` against a unit whose
        `ConditionPathIsMountPoint = ${mountPoint}` still holds
        (because the umount failed) would (a) drive the unit
        into a stop transition that immediately fires a new
        `braid lock --systemd-stop` reentry against the
        still-mounted pool, and (b) leave `braid-online.service`
        in `inactive` while the pool is still up -- making a
        subsequent `systemctl start braid-online.service`
        succeed and re-arm a shutdown hook that races the next
        unlock's `mark_online`. Both are correctness regressions.
      - Return the underlying `Err`. The coordinator and pool
        guards both drop on return. Any concurrent ExecStop
        reentry's poll loop will then observe coord-release with
        empty content (no `done\n`), take the `Acquired` branch,
        and run its own `cmd_lock` -- which will likely fail the
        same way, expire the deadline, and report the unit
        failed. That is the correct end state: systemd records a
        failed stop, the operator sees the failure in the
        journal, and the pool is still mounted (recoverable).
   4. Write `done\n` to the coordinator file via a single
      `write()` call (atomic w.r.t. concurrent readers of a
      regular file at a fixed offset). `fsync` is not required
      -- this is `/run`, a tmpfs. Reached only on `cmd_lock`
      success.
   5. Call `mark_offline`, which does synchronous
      `systemctl stop braid-online.service`. The unit's ExecStop
      fires `braid lock --systemd-stop`; the reentry's poll sees
      `done\n` and exits 0; `systemctl stop` returns when the
      unit reaches `inactive`. Reached only on `cmd_lock`
      success.
   6. Drop the coordinator guard. (The file content remains until
      the next acquirer's `ftruncate`; that's fine because no one
      else can hold the coordinator while we do.) Drop the pool
      lock.

The asymmetry in step 2.4 vs step 3.5's `mark_offline` matches
today's `BRAID_SYSTEMD_EXECSTOP=1`-gated wrapper logic
(`modules/braid/braid-wrapper.sh:162-167` and decision 018 line 131):
plain `braid lock` deactivates the unit synchronously; the ExecStop
reentry never does.

**Why coordinator-first, then pool lock.** Plain's race with an
externally-triggered ExecStop hinges on which order observers see.
If plain acquired the pool lock first and then the coordinator, a
window exists where plain holds the pool lock but not the
coordinator, and a concurrent external `systemctl stop` could fire
ExecStop, see the coordinator available, acquire it, then block on
the pool lock that plain holds -- and plain's eventual synchronous
`systemctl stop` would wait for that ExecStop to finish. Three-way
deadlock until the deadline. Acquiring the coordinator first
guarantees the ExecStop's probe ALWAYS observes plain's claim on
the stop transition, so the reentry takes the `Held` branch (and
polls for `done` or release) instead of the `Acquired` branch
(which would block on the pool lock).

**Why polling, not blocking.** When the reentry sees `Held`, it
needs to wait for one of two events: plain finishes (`done`
appears) or plain crashes (flock released). Both are observable
without holding any blocking primitive against plain. Blocking
`flock` on the coordinator instead would deadlock because plain
holds the coordinator across the synchronous `systemctl stop`
that triggered the reentry in the first place.

**Why this beats a mount-only fast-path.** An earlier draft fast-
pathed on `!is_mountpoint(mount_point)`, but that conflates "we just
unmounted under the pool guard" with "out-of-band unmount happened
and `braid-online.service` is stale." Decision 018:181 makes the
second case `braid lock`'s responsibility to clean up, so the fast-
path must NOT short-circuit it. The coordinator + `done` content
distinguishes the two cases without losing the cleanup guarantee.

**ExecStop scenarios under this design:**

| Scenario                                                              | Initial coord probe | done content | Action taken                                                                |
| --------------------------------------------------------------------- | ------------------- | ------------ | ---------------------------------------------------------------------------- |
| System shutdown (pool mounted, no plain `braid lock` in flight)       | acquired            | (n/a)        | full `cmd_lock` (umount, close LUKS) under bounded-wait pool lock          |
| System shutdown while a mutator holds the pool lock                   | acquired            | (n/a)        | bounded-wait acquire pool lock; full `cmd_lock` after mutator release       |
| Manual `systemctl stop` after out-of-band unmount + orphan mappers    | acquired            | (n/a)        | full `cmd_lock` (`Snapshot::Unmounted`, closes orphan mappers)              |
| Recursive ExecStop from plain `braid lock`'s synchronous stop          | held                | `done\n`     | exit 0 immediately                                                          |
| Manual `systemctl stop` races a slow plain `braid lock` (pre-cmd_lock) | held                | empty -> `done` | poll until `Done`; exit 0. plain finishes its own cleanup and synchronous stop. |
| Plain `braid lock` crashes mid-`cmd_lock`                              | held                | empty        | poll detects coord release; reentry acquires + runs full `cmd_lock`         |
| Plain `braid lock` hangs past deadline                                 | held                | empty        | poll returns `Deadline`; reentry prints `DeadlineExpired`, exits 1          |

Clap unit tests:
- `lock_plain_parses_without_systemd_stop`
- `lock_dry_run_parses`
- `lock_systemd_stop_with_deadline_parses`
- `lock_systemd_stop_without_deadline_rejected`
- `lock_deadline_without_systemd_stop_rejected`
- `lock_deadline_zero_rejected`

`lock.rs` plain-path failure unit tests (use `RecordingPoolLock` +
`RealStopCoordinator` rooted in a `tempfile::tempdir` +
`RecordingOnlineStateOps`; stub `cmd_lock` to return `Err(...)`
via the existing `cmd_lock` seam):

The coordinator is `RealStopCoordinator::new(tempdir.path()
.join("coord"))`, NOT a recording fake. This matches the
convention already used by the stop-coordinator state-machine
tests in §"`pool_lock.rs` (stop-coordinator surface)" below,
which exercise `RealStopCoordinator` against tempdir-rooted
files so the kernel `flock(2)` semantics are not mocked away.
There is no `RecordingStopCoordinator`; the coordinator's
behavior is "write bytes to a file under an `flock`," and a
fake for that primitive would only re-implement the kernel.

- `cmd_lock_failure_does_not_write_done_or_stop_online` --
  plain arm runs to step 3.3 with a stubbed `cmd_lock` that
  returns `Err(LockError::UmountFailed { .. })` (or any
  variant). Asserts:
  (a) the coordinator file content is empty (no `done\n` was
      written). Read the file directly via
      `std::fs::read(tempdir.path().join("coord"))` after the
      plain arm returns; the coordinator guard has dropped by
      then but the file persists in the tempdir.
      `assert!(bytes.is_empty())`.
  (b) no `SystemctlStop { unit: "braid-online.service",
      no_block: false }` argv is recorded against
      `RecordingOnlineStateOps`.
  (c) `cmd_lock`'s underlying `Err` is propagated out of the
      plain arm (the caller can match on the original variant
      for exit code classification).
  This is the regression gate for the "do not lie to the
  ExecStop reentry; do not deactivate a unit whose mountpoint
  condition still holds" rule documented in step 3.3 above.
- `cmd_lock_success_writes_done_then_calls_mark_offline_in_order`
  -- positive companion. `cmd_lock` returns `Ok(_)`. Pins the
  ordering "plain writes `done\n` BEFORE calling `systemctl
  stop`" so an ExecStop reentry that races plain's
  synchronous stop will always observe the marker. Pure
  end-state inspection cannot prove ordering on its own (both
  side effects are visible by the time the plain arm
  returns), so `RecordingOnlineStateOps::systemctl_stop`
  snapshots the coordinator-file content at call time and
  stores it alongside the recorded argv. The test:
  1. constructs `RealStopCoordinator::new(tempdir.path()
     .join("coord"))` and a `RecordingOnlineStateOps`
     configured with `coord_file_path =
     tempdir.path().join("coord")` (the recording fake reads
     the file each time `systemctl_stop` fires and records
     the bytes).
  2. runs the plain arm with `cmd_lock` stubbed to `Ok(_)`.
  3. asserts the final `RecordingOnlineStateOps` call log
     contains a single `SystemctlStop { unit:
     "braid-online.service", no_block: false, observed_coord:
     bytes_at_call_time }` and that
     `observed_coord == b"done\n"`. If a regression reorders
     `mark_offline` ahead of the `done\n` write,
     `observed_coord` will be empty and the test fails.
  4. additionally asserts the post-return file content is
     `done\n` (defensive end-state check).
  The instrumentation hook (`coord_file_path` on the
  recording fake) is only used by these two tests; it does
  not appear on the real `OnlineStateOps` trait.

### `CmdRequest` additions in `cli/src/cmd.rs`

Reused (already present in `cli/src/cmd.rs`):

- `SystemctlShowActiveState { unit }` (cli/src/cmd.rs:319; `to_argv`
  at line 1099, asserted by the variant's existing argv test around
  cli/src/cmd.rs:1750). Used by `online_state` for the snapshot. Do
  NOT redefine -- a duplicate variant fails to compile, and a
  parallel "ShowActiveState" name would diverge from the existing
  call sites. (Note: `cli/src/cmd.rs` has no `SystemctlIsActive`
  variant; `systemctl is-active` is the wrong source of truth for
  the snapshot anyway because it conflates `Activating` and
  `Deactivating` with "not active." `SystemctlShowActiveState`
  reads `ActiveState` directly and is the right primitive.)

New typed variants (with `to_argv` unit tests per the existing
pattern):

- `SystemctlStart { unit }` -> `["systemctl", "start", "<unit>"]`.
- `SystemctlStop { unit, no_block: bool }` -> `["systemctl", "stop",
  ("--no-block")?, "<unit>"]`. The `no_block` field exists so
  `cmd_lock`'s `mark_offline` can pass `false` (synchronous) and any
  future caller that needs queued semantics can pass `true` without
  inventing a second variant.
- `SystemctlShowBoundBy { unit }` -> `["systemctl", "show", "-P",
  "BoundBy", "<unit>"]`. Mirrors the existing
  `SystemctlShowActiveState` shape so the BoundBy lookup goes
  through the typed `CommandRunner` seam instead of a hand-rolled
  argv at the call site. The variant is required by
  `cmd_lock`'s BoundBy consumer-stop pre-step (see "`cmd_lock` --
  inline the wrapper's scrub-stop and BoundBy stop pre-steps"
  below); without a typed variant the implementation either skips
  the consumer stop (which loses today's EBUSY-on-umount
  protection) or hand-rolls the argv (which bypasses
  `RecordingRunner` and makes the BoundBy parsing untestable in
  the same lane as the rest of `lock.rs`). Stdout is the
  whitespace-separated unit list that
  `OnlineStateOps::list_bound_by(unit)` parses into `Vec<String>`
  (single space and newline are both valid separators per
  `systemd.unit(5)`; `str::split_whitespace` is the right
  primitive).

### Wrapper

`modules/braid/braid-wrapper.sh` reduces to:

```sh
#!@shell@
export PATH="@toolPath@:$PATH"
exec @braidBin@ "$@"
```

`modules/braid/wrapper.nix` drops the substitutions that the new
wrapper no longer uses: `flockBin`, `mountpointBin`, `chownBin`,
`chmodBin`, `systemctlBin`, `mountPointPath`, `storageGroup`. Only
`shell`, `braidBin`, `toolPath` remain.

`modules/braid/cli.nix` extends the generated JSON config to include
`storage_group` (move from wrapper substitution to runtime config).
`Config` in `cli/src/config.rs` gains a `storage_group: Option<String>`
field. `mount_point` is already there (`add.rs:562` references
`self.config.mount_point()`).

### NixOS module option + assertion

**Single-source the unit's stop timeout** so the assertion, the
assertion message, and the unit's `TimeoutStopSec=` can never
desync. Today's `modules/braid/storage.nix:144` hardcodes
`TimeoutStopSec = "5min";`; the plan would introduce a new `300`
literal in the assertion. A future operator who wanted a longer
deadline would have to remember to update three places.

In `modules/braid/options.nix` introduce one internal constant
plus the deadline option, both expressed in whole seconds:

```nix
# Internal constant (not user-facing). The braid-online.service
# unit's TimeoutStopSec lives here so the eval-time assertion and
# the unit definition share one source of truth.
braidOnlineStopTimeoutSecs = 300;  # 5 minutes
```

(Place this in a `let` block at the top of `options.nix`, or in a
new tiny `modules/braid/constants.nix` and `import` it from both
`options.nix` and `storage.nix` -- whichever the implementer
finds cleaner; the requirement is just that there is one
source.)

Then the option and the assertion both reference the constant:

```nix
lockSystemdStopDeadlineSecs = lib.mkOption {
  type = lib.types.ints.positive;
  default = 270;
  description = ''
    Seconds to wait for /run/braid-pool.lock during
    braid-online.service ExecStop. Must be strictly less than
    braid-online.service TimeoutStopSec (${toString braidOnlineStopTimeoutSecs} seconds)
    so braid returns DeadlineExpired before systemd's own
    SIGKILL fires.
  '';
};
```

Eval-time assertion (added to the existing `assertions` block):

```nix
{
  assertion = cfg.lockSystemdStopDeadlineSecs < braidOnlineStopTimeoutSecs;
  message = "braid.lockSystemdStopDeadlineSecs (${toString cfg.lockSystemdStopDeadlineSecs}) must be strictly less than braid-online.service TimeoutStopSec (${toString braidOnlineStopTimeoutSecs}).";
}
```

The message is deliberately a **single-line** Nix string (`"..."`,
not `''...''`). The negative eval test compares
`a.message == expectedMessage` byte-for-byte; a `''...''`
multi-line literal would embed a newline between
`(${...})` and `must be` (Nix's `''...''` strips common leading
indentation but preserves internal newlines), and the comparison
would fail even when the assertion fires. Keep this literal as one
line, and keep the negative test's `expectedMessage` as the same
one-line text (see "Eval tests" below).

In `modules/braid/storage.nix:144` change `TimeoutStopSec = "5min";`
to use the constant:

```nix
TimeoutStopSec = "${toString braidOnlineStopTimeoutSecs}s";
```

(`5min` and `300s` are equivalent systemd time syntax per
`systemd.time(7)`.)

In `modules/braid/storage.nix:141` change:

```nix
ExecStop = "${braidWrapped}/bin/braid lock --systemd-stop --deadline-secs ${toString cfg.lockSystemdStopDeadlineSecs}";
```

Drop the `BRAID_SYSTEMD_EXECSTOP=1` env var entirely (Rust now keys
on `args.systemd_stop`).

### `cmd_lock` -- inline the wrapper's scrub-stop and BoundBy stop pre-steps

Today the wrapper does two pre-CLI things before invoking
`@braidBin@` for `lock`:

1. **Scrub-stop** (`modules/braid/braid-wrapper.sh:88-96`): stops
   `braid-scrub.timer`, then `braid-scrub-resume-trigger.service`,
   then `braid-scrub.service`. Each call uses `2>/dev/null || true`
   to silently swallow failures, because these units do not exist
   when `autoScrub` is disabled, and there is no actionable
   information in "stop failed because unit doesn't exist."
2. **BoundBy consumer-stop**
   (`modules/braid/braid-wrapper.sh:115-132`): iterates
   `systemctl show -P BoundBy braid-online.service`, skips the three
   scrub units already handled above, stops each remaining unit
   (samba, nfs, future). Failures are surfaced with
   `braid: WARNING: failed to stop $unit (exit $ec) -- continuing;
   umount may fail` -- the wrapper deliberately warns here because
   anything BoundBy is expected to exist.

Move both into `cmd_lock` as the first steps inside the lock-held
critical section, using the new `SystemctlStop` variant for the
stop and the new `SystemctlShowBoundBy` variant (via
`OnlineStateOps::list_bound_by`) for the BoundBy lookup. Both go
through `CommandRunner` so `RecordingRunner` captures every
issued argv in `lock.rs`'s unit tests.

**Preserve the warning-semantics asymmetry exactly:**

- **Scrub-stop:** silent ignore on nonzero exit (matches today's
  `2>/dev/null || true`). Do NOT warn -- when `autoScrub` is
  disabled, every lock would emit three spurious WARNING lines.
- **BoundBy consumer-stop:** emit
  `braid: WARNING: failed to stop $unit (exit $ec) -- continuing;
  umount may fail` on nonzero exit (matches today's wrapper).

The asymmetry is encoded in two distinct helpers (e.g.
`stop_unit_silent_if_missing` vs `stop_unit_warn_on_error`) so a
future caller cannot accidentally collapse them.

**`lock.rs` unit tests for the BoundBy pre-step (using
`RecordingPoolLock` + `RecordingRunner` + `RecordingOnlineStateOps`
in the same shape as existing `lock.rs` tests):**

- `bound_by_pre_step_skips_three_scrub_units` -- stub
  `list_bound_by("braid-online.service")` to return
  `["braid-scrub.timer", "braid-scrub.service",
   "braid-scrub-resume-trigger.service", "smbd.service",
   "nfs-server.service"]` (whitespace-delimited input parsed via
  the trait); assert `cmd_lock` issues `SystemctlStop` ONLY for
  `smbd.service` and `nfs-server.service` (not the three scrub
  units, which are handled by the scrub-stop block above).
- `bound_by_pre_step_warns_on_nonzero_stop` -- stub
  `OnlineStateOps::systemctl_stop("smbd.service", false)` to
  return `Err(OnlineError::SystemctlStop { exit_code: 5, ... })`;
  assert the captured stderr contains the exact
  wrapper-compatible warning prefix
  `braid: WARNING: failed to stop smbd.service (exit 5) --
  continuing; umount may fail`, and that `cmd_lock` continues
  rather than returning early.
- `bound_by_pre_step_silently_continues_when_list_bound_by_errs`
  -- stub `list_bound_by` to return
  `Err(OnlineError::SystemctlShow { exit_code: 1, .. })`; assert
  `cmd_lock` proceeds to umount, issues no `SystemctlStop`,
  prints no stderr line whatsoever (no "WARNING:" prefix, no
  contention text -- the lookup failure is the only case where
  `cmd_lock` swallows the `Err` silently to match today's
  wrapper `2>/dev/null || true` on `systemctl show -P BoundBy`).
- `bound_by_pre_step_handles_empty_bound_by_property` -- stub
  `list_bound_by` to return `Ok(Vec::new())` (the property is
  empty: no consumers declared `BindsTo=braid-online.service`);
  assert `cmd_lock` issues no `SystemctlStop` and proceeds to
  umount without warning. This is the positive companion to the
  error case above; the trait split (`Ok(empty)` vs `Err`)
  exists precisely so the two cases are distinguishable, and the
  tests pin the distinction.
- `bound_by_pre_step_parses_whitespace_separated_units` --
  exercises `RealOnlineStateOps::list_bound_by` directly (not
  the recording fake) with a `RecordingRunner` whose stdout
  fixture is real `systemctl show -P BoundBy` output (single
  line, space-separated, no trailing newline); asserts the
  returned `Ok(Vec<String>)` matches the parsed units. Variant
  fixtures, each its own `#[test]` so a single regression
  doesn't mask the rest:
  - empty stdout -> `Ok(Vec::new())` (`BoundBy=` is empty;
    `cmd_lock`'s pre-step does nothing).
  - trailing newline -> still `Ok(units)` (defensive parse
    against the manpage's unspecified trailing whitespace).
  - `RecordingRunner` returns
    `Ok(RawCommandOutput { exit_status: 1, .. })` -> asserts
    `Err(OnlineError::SystemctlShow { exit_code: 1, .. })`
    (the runner-ran-systemctl-said-no branch).
  - `RecordingRunner` returns
    `Err(CmdError::Failed("..."))` -> asserts
    `Err(OnlineError::Spawn { source: CmdError::Failed(..) })`
    (the runner-couldn't-run-systemctl branch; pins the
    distinct variant guaranteed by the trait doc).
  No UTF-8-invalid fixture -- the shared `output_to_raw`
  decodes with `from_utf8_lossy` before stdout reaches this
  seam (`cli/src/cmd.rs:1199`), so the real failure mode would
  never reach `list_bound_by` as an error and the test would
  have to bypass `CommandRunner` to construct it.
- `scrub_stop_pre_step_swallows_missing_unit` -- stub
  `systemctl_stop("braid-scrub.timer", false)` to return
  `Err(...)`; assert no warning is emitted and `cmd_lock`
  continues (regression gate for the existing
  `2>/dev/null || true` semantic).

**VM regression gate (already registered, must keep passing
unchanged):**
`tests/module/lock-stops-bound-consumers.py` (+ `.nix`,
registered in `flake.nix:836-837`). Boots a VM with a BoundBy
consumer that holds `/mnt/storage` busy, then asserts `braid
lock` stops the consumer before umount and succeeds without
EBUSY. This is the cross-check that the typed seam reproduces
today's wrapper behavior end-to-end. The plan adds it to the
"Existing tests that must continue to pass without change" list
under "Tests" below so the migration cannot silently drop the
BoundBy pre-step.

## Files

### New

- `cli/src/pool_lock.rs` -- `AcquirePoolLock` trait, `RealPoolLock`,
  `RecordingPoolLock`, `PoolLockError`.
- `cli/src/online_state.rs` -- `OnlineStateOps` trait,
  `RealOnlineStateOps`, `RecordingOnlineStateOps`, `snapshot`,
  `mark_online`, `mark_offline`.
- `docs/decisions/026-pool-lock-rust-owned.md` -- new ADR, status
  `Active`. Records the structural decision and supersedes the
  relevant prose in 018. The slot is 026 because 021-025 are already
  in use (`021-wait-in-unlock.md`, `022-dry-run-preview-model.md`,
  `023-secret-handling.md`, `024-luks-uuid-identity.md`,
  `025-browse-vs-curated.md`).
- `tests/module/braid-lock-systemd-stop.py` (+ `.nix`) -- bounded-wait
  happy path and deadline expiry path.
- `tests/module/pool-lock-enroll-contention.py` (+ `.nix`) -- enroll
  fail-fast against held lock.
- `tests/module/pool-lock-lock-contention.py` (+ `.nix`) -- user-
  initiated `braid lock` fail-fast against held lock.
- `tests/module/pool-lock-precedes-state-read.py` (+ `.nix`) --
  **mandatory** registered VM test (real binary) enforcing the
  lock-before-state-read invariant for every locked mutator. See
  "Tests" below for the per-arm subtest list.
- `tests/module/braid-pool-lock-not-inherited.py` (+ `.nix`) --
  REPLACES `wrapper-pool-lock-not-inherited.py`. Asserts the braid
  binary owns fd on `/run/braid-pool.lock`; descendants do not
  inherit it.
- `tests/module/braid-pool-lock-released-after-sigkill.py` (+
  `.nix`) -- REPLACES `wrapper-pool-lock-released-after-sigkill.py`.
  Asserts the lock is released on SIGKILL of the braid binary.
- `tests/eval/_braid-eval-harness.nix` -- shared parameterized
  harness (`{ linuxPkgs, nixpkgs, linuxSystem, lockSystemdStopDeadlineSecs }`)
  that imports `../../modules/braid` with a minimal-enabled
  config: `braid.enable = true`; stub
  `braid.package = linuxPkgs.writeShellScriptBin "braid" "exit 0"`;
  the deadline under test; `system = linuxSystem`. Used by both
  positive and negative eval test files; threads `linuxSystem`
  from `checksFor` so the NixOS evaluation targets the right
  architecture on both `x86_64-linux` and `aarch64-darwin` (via
  `aarch64-linux`).
- `tests/eval/lock-systemd-stop-deadline-assertion.nix` -- positive
  eval-time-assertion case (registered as flake check
  `eval-lock-systemd-stop-deadline-ok`).
- `tests/eval/lock-systemd-stop-deadline-assertion-fails.nix` --
  expected-eval-failure case (registered as flake check
  `eval-lock-systemd-stop-deadline-fails`).
- `tests/module/braid-lock-then-unlock-no-race.py` (+ `.nix`) --
  F2 regression gate: synchronous post-lock stop. See "Tests" below.
- `tests/module/execstop-cleans-stale-online.py` (+ `.nix`) --
  F2-#2 regression gate: stale `braid-online.service` + out-of-band
  unmount + open mappers. The `--systemd-stop` reentry must NOT
  fast-path; it must acquire the pool lock and run `cmd_lock` to
  close the orphan mappers. See "Tests" below.
- `tests/module/braid-lock-coordinator-race.py` (+ `.nix`) --
  F1 (third-round) regression gate: external `systemctl stop
  braid-online.service` overlapping a slow plain `braid lock`.
  Verifies the stop-coordinator design prevents the three-way
  deadlock the polling design dissolves. See "Tests" below.
- `tests/module/mark-online-skips-start-while-deactivating.py`
  (+ `.nix`) -- VM regression gate for the `Deactivating`
  snapshot rule end-to-end. Asserts the snapshot site sits at
  dispatch (after pool-lock acquisition, before mutation) and that
  a future refactor cannot silently move it without failing this
  test. See "Tests" below.

### Modified

- `cli/Cargo.toml` -- add `nix = { version = "...", features = ["fs",
  "user"] }`.
- `cli/src/lib.rs` -- declare the two new modules: `pub mod
  pool_lock;` and `pub mod online_state;`. The plan creates
  `cli/src/pool_lock.rs` and `cli/src/online_state.rs` under "New
  files" above, and main.rs uses them through `braid_cli::`, but
  Rust requires the explicit `pub mod` registration in
  `cli/src/lib.rs` (alongside the other `pub mod` declarations at
  lines 1-61). Without this line, the modules are unreachable at
  `main.rs` and compilation fails -- compile-caught, but still on
  the implementation checklist so it is not forgotten.
- `cli/src/main.rs` -- dispatch acquires the pool lock at the top of
  every non-dry-run mutator arm; `LockArgs` gains `systemd_stop` and
  `deadline_secs`; `cmd_*` signatures take already-loaded config;
  `cmd_lock` two-mode dispatch.
- `cli/src/cmd.rs` -- new `SystemctlStart` and `SystemctlStop`
  variants with `to_argv` tests. `SystemctlShowActiveState` is
  reused as-is (already at line 319).
- `cli/src/config.rs` -- add `storage_group: Option<String>`.
- `cli/src/lock.rs` -- `cmd_lock` inlines scrub-stop, BoundBy
  consumer-stop, and the post-success `mark_offline` call.
- `cli/src/unlock.rs`, `cli/src/add.rs`, `cli/src/recover.rs` -- each
  takes already-loaded config; main.rs calls `mark_online` after
  success.
- `cli/src/remove.rs`, `cli/src/remove_missing.rs`,
  `cli/src/replace.rs`, `cli/src/enroll_key_file.rs`,
  `cli/src/discover.rs` -- each takes already-loaded config.
- `modules/braid/braid-wrapper.sh` -- reduce to PATH + exec.
- `modules/braid/wrapper.nix` -- drop unused substitutions.
- `modules/braid/cli.nix` -- extend JSON config with `storage_group`.
- `modules/braid/options.nix` -- add `lockSystemdStopDeadlineSecs` +
  eval-time assertion.
- `modules/braid/storage.nix:141` -- new ExecStop form; drop
  `BRAID_SYSTEMD_EXECSTOP=1` env var.
- `tests/module/pool-lock-discover-contention.py` (+ `.nix`) --
  revise: bare `discover` no longer fails on contention; assert
  `pool.json` unchanged instead. See "Tests to revise" below.
- `flake.nix` -- update `checks` via `checksFor`: remove the
  obsolete `wrapper-pool-lock-*` checks; add the new
  `braid-pool-lock-*`, `braid-lock-systemd-stop`,
  `braid-lock-then-unlock-no-race`,
  `execstop-cleans-stale-online`,
  `pool-lock-{lock,enroll}-contention`,
  `pool-lock-precedes-state-read`,
  `braid-lock-coordinator-race`,
  `mark-online-skips-start-while-deactivating`,
  `eval-lock-systemd-stop-deadline-ok`, and
  `eval-lock-systemd-stop-deadline-fails` checks.
- `docs/principles.md:67` -- add `lock` and `enroll` to Principle 12.
- `docs/decisions/018-systemd-lifecycle.md` -- update §"CLI wrapper
  as synchronization layer" and §"Pool lock mutual exclusion" to
  reflect Rust-side ownership; cross-link to ADR 026.
- `docs/index.md` -- add a new bullet for
  `decisions/026-pool-lock-rust-owned.md` under the `decisions/`
  list (one-line summary, matches the style of the existing ADR
  entries).

## Tests

Unit (Rust):

- `pool_lock.rs` (pool-lock surface):
  - `already_held_display_is_wrapper_compatible_verbatim` -- asserts
    the exact contention line.
  - `deadline_expired_display_distinguishes_from_already_held` --
    contains `--systemd-stop` or `deadline` substring.
  - `acquire_returns_already_held_on_second_attempt`
    (`RecordingPoolLock`).
  - `acquire_with_timeout_polls_then_succeeds` -- holder released
    mid-timeout.
  - `acquire_with_timeout_returns_already_held_on_expiry` --
    holder never releases; error is `AlreadyHeld` (ack's shape).
  - `acquire_with_systemd_stop_deadline_polls_then_succeeds`.
  - `acquire_with_systemd_stop_deadline_returns_deadline_expired_on_expiry`
    -- error is `DeadlineExpired { waited }` (--systemd-stop's
    distinct shape).
- `pool_lock.rs` (stop-coordinator surface): the
  `RealStopCoordinator` state machine has three terminal poll
  outcomes (`Done`, `Acquired`, `Deadline`) plus initial-acquire
  semantics. Each must be unit-tested with a real `tempdir`-rooted
  coordinator file so the kernel `flock` semantics are exercised
  (not mocked away).
  - `stop_coordinator_acquire_then_second_acquire_returns_held` --
    fork two `RealStopCoordinator` instances pointing at the same
    file; first `acquire` succeeds, second `acquire` returns
    `Held`.
  - `stop_coordinator_acquire_truncates_stale_done` -- pre-seed
    the file with `done\n` bytes (simulating a crashed
    predecessor); `acquire` succeeds and reads back an empty
    file. Asserts both the truncate and that the held guard sees
    no stale content.
  - `stop_coordinator_poll_returns_done_while_holder_still_holds` --
    one thread/process holds the coordinator and writes `done\n`;
    a second instance's `poll_for_done_or_release(deadline)`
    returns `Done` BEFORE the holder drops. This is the critical
    deadlock-avoidance assertion: the poll must observe the
    content while the flock is still held.
  - `stop_coordinator_poll_returns_acquired_after_holder_releases_without_done`
    -- holder drops without writing `done\n` (simulating a
    crash); poller's next tick returns
    `Acquired(StopCoordinatorGuard)`.
  - `stop_coordinator_poll_returns_deadline_when_held_with_empty_content`
    -- holder holds indefinitely, never writes `done\n`; poll
    with a short deadline (e.g. 200 ms) returns `Deadline`.
  - `stop_coordinator_guard_release_lets_next_acquire_succeed` --
    after the guard's `Drop`, the kernel flock is released and a
    fresh `acquire` on a new instance succeeds. Closes the loop on
    `OwnedFd`-on-drop semantics.

  Implementation note for these tests: spawn a child process (or
  use a separate thread with its own `OwnedFd`) for the "other
  holder" so the flock is actually held against two file
  descriptions, not one. `cfg(target_os = "linux")` if
  Linux-only `flock` semantics
  are required for the assertion to be meaningful; otherwise gate
  the whole module on Linux per existing conventions in
  `cli/src/inhibit.rs`.
- `online_state.rs`:
  - Snapshot: each `UnitActiveState` parsed correctly from
    `systemctl show -P ActiveState` output, including
    `active`/`activating`/`deactivating`/`inactive`/`failed`/
    `reloading`/`refreshing` and the synthesized `Unknown(_)` on
    parse failure. The `refreshing` case is one of its own
    `#[test]`s so the implementer doesn't silently route it
    through `Unknown` (which would emit the new mounted-pool
    warning unnecessarily, per the start-gate rule above).
  - `mark_online_skips_start_on_refreshing_silently` -- snapshot
    is `UnitActiveState::Refreshing`, `is_mountpoint` returns
    `true`; assert no `SystemctlStart` argv is recorded AND
    stderr contains no warning prefix (the active-like
    classification means a start would be redundant, not an
    error; the warning belongs only to `Unknown(_)`).
  - `snapshot_records_unknown_on_systemctl_show_failure` -- stub
    `unit_active_state` to return `Err(...)`; assert the returned
    `OnlineSnapshot.online_state == UnitActiveState::Unknown(...)`
    (snapshot is infallible).
  - `mark_online`:
    - Skips `chown`/`chmod` when `storage_group = None`.
    - Issues `systemctl start` only when snapshot was `Inactive` or
      `Failed`; asserts the full snapshot -> start matrix.
    - `mark_online_warns_on_unknown_with_mounted_pool` -- snapshot
      is `UnitActiveState::Unknown("io error: ...")`,
      `is_mountpoint` returns `true`; assert (a) `systemctl_start`
      is NOT issued (no `SystemctlStart` argv recorded), and (b)
      stderr contains the exact line
      `braid: WARNING: could not read braid-online.service
      ActiveState (io error: ...) -- pool is mounted but shutdown
      may not lock automatically`. The substring "shutdown may not
      lock automatically" matches today's wrapper warning so a
      grep over operator transcripts continues to surface the same
      class of incident.
    - `mark_online_silent_on_unknown_with_unmounted_pool` --
      snapshot is `Unknown(_)`, `is_mountpoint` returns `false`;
      assert no warning is printed and no `systemctl_start` is
      issued (step 1 short-circuits).
    - Each step's failure logs WARNING and returns `Ok(())`.
  - `mark_offline`:
    - Skips `systemctl stop` when path is still a mountpoint.
    - Runs synchronous `systemctl stop braid-online.service`
      (`no_block = false`) unconditionally otherwise; asserts the
      argv contains no `--no-block` flag (regression gate against
      the async-stop race).
    - Warn-only on nonzero exit.
- `main.rs` (clap):
  - The six `LockArgs` parse tests listed above.

VM (Python under `tests/module/`):

- `braid-lock-systemd-stop.py` -- two subtests:
  1. **Happy path:** start a long-running `braid add` (urandom payload
     triggers the post-add balance). Wait for
     `flock -n /run/braid-pool.lock true` to fail (lock held).
     `systemctl stop braid-online.service` (synchronous). Assert:
     stop completes after `add` finishes, well under
     `TimeoutStopSec`. Journal contains no `DeadlineExpired` line.
  2. **Deadline path:** set
     `braid.lockSystemdStopDeadlineSecs = 5` via test-only NixOS
     config. Hold the lock externally with the
     `nohup flock -x ... sh -c 'touch /tmp/holder.ready; sleep 30'`
     idiom. `systemctl stop braid-online.service`. Assert: stop exits
     nonzero before `TimeoutStopSec`; journal contains the
     `DeadlineExpired` Display string (the `--systemd-stop` /
     `deadline` substring). Do NOT assert that systemd "takes over."
- `pool-lock-enroll-contention.py` -- enroll fail-fast against held
  lock; mirrors `pool-lock-contention.py`.
- `pool-lock-lock-contention.py` -- user-initiated `braid lock`
  fail-fast against held lock.
- `pool-lock-precedes-state-read.py` (+ `.nix`) -- **MANDATORY**
  registered VM test against the real braid binary. Enforces the
  "lock-before-state-read" invariant stated in the dispatch wiring
  section (lock is acquired before config / membership / journal /
  probe / prompt I/O).

  Strategy: hold `/run/braid-pool.lock` externally, then invoke a
  set of representative mutators with **deliberately broken
  pre-lock inputs** (the kind of inputs that would produce an error
  message if any state read ran before the lock acquire). For each
  mutator, the canonical contention line MUST appear and the
  broken-input error MUST NOT appear, proving the lock acquire
  happened first. The set covers every commit boundary so a new
  mutator added later cannot regress.

  Concrete subtests -- one per locked dispatch arm, covering
  every command in the per-command Acquisition-mode table.
  All use syntactically valid command forms against the real CLI
  grammar in `cli/src/main.rs` so clap will not reject them
  BEFORE dispatch reaches the lock-acquisition site.

  **Fail-fast `AlreadyHeld` arms** (`unlock`, `add`, `recover`,
  `remove`, `remove-missing`, `replace`, `enroll`, plain `lock`,
  `discover --write`). Each subtest's assertion shape: `rc != 0`,
  output contains `"another braid operation is already in
  progress"`, output does NOT contain `"No such file or
  directory"`, `"Configuration file not found"`, `"Usage:"` (clap
  usage error -- defensive check that the command form was
  accepted by clap before the lock check fired), or any other
  config-load / pool-state diagnostic.

  1. `braid --config /nonexistent/braid.json unlock
     --passphrase-stdin </dev/null`
     -- `UnlockArgs` accepts `--passphrase-stdin` from
     `PassphraseInputArgs`.
  2. `braid --config /nonexistent/braid.json add
     disk1=/dev/disk/by-id/virtio-disk1 --passphrase-stdin --yes
     </dev/null`
     -- `AddArgs` accepts positional `name=path` disk specs plus
     `--passphrase-stdin` and `--yes` (from `CommonArgs` at
     `cli/src/main.rs:135-146`).
  3. `braid --config /nonexistent/braid.json recover
     --passphrase-stdin </dev/null`
     -- `RecoverArgs` (cli/src/main.rs:109-126) takes
     `--passphrase-stdin` via `PassphraseInputArgs`; no positional.
  4. `braid --config /nonexistent/braid.json remove disk1 --yes`
     -- `RemoveArgs` (cli/src/main.rs:193-200) takes a positional
     disk name and `--yes` via `CommonArgs`.
  5. `braid --config /nonexistent/braid.json remove-missing
     --missing-id 1 --yes`
     -- `RemoveMissingArgs` (cli/src/main.rs:202-209) requires
     `--missing-id u64`; `--yes` comes from `CommonArgs`.
  6. `braid --config /nonexistent/braid.json replace
     --old disk1 --new disk2=/dev/disk/by-id/virtio-disk2
     --passphrase-stdin --yes </dev/null`
     -- `ReplaceArgs` (cli/src/main.rs:211-231) requires `--old`
     and `--new` (both `long`-style flags). The `--new` value is
     a `name=path` pair per the existing
     `pool-lock-replace-contention.py:60` usage. Using positional
     `old-name` would have been rejected by clap.
  7. `braid --config /nonexistent/braid.json enroll
     /nonexistent/keydir --passphrase-stdin </dev/null`
     -- `EnrollKeyFileArgs` (cli/src/main.rs:264-276) takes a
     positional `dir: PathBuf`, not a `--key-file` flag.
  8. `braid --config /nonexistent/braid.json lock`
     -- `LockArgs` (cli/src/main.rs:128-133) plus the new
     `--systemd-stop` / `--deadline-secs` hidden flags (not
     exercised here; this subtest covers the plain user-facing
     mode).
  9. `braid discover --write --expect-count 1`
     -- `DiscoverArgs` (cli/src/main.rs:286-299).

     **`--config` is dropped from this subtest's invocation**
     because the `Commands::Discover` dispatch arm at
     `cli/src/main.rs:773-835` does NOT call `config_read`
     (compare every other locked-mutator arm in
     `cli/src/main.rs:512-835`, which calls `config_read` after
     pool-lock acquire). Pointing at `/nonexistent/braid.json`
     therefore proves nothing about ordering for discover -- the
     config is never read regardless of when the lock is
     acquired. Replace the broken-config trigger with a
     pending-journal trigger that the dispatch arm DOES
     actually touch in the pre-lock window if the invariant
     regresses:

     1. Before holding the external pool lock, pre-seed
        `/var/lib/braid/pending-op.json` with any non-empty
        bytes:

        ```sh
        printf '{"op":"placeholder"}' \
            > /var/lib/braid/pending-op.json
        ```

        `write_discovered_membership` checks
        `journal_path.exists()` as its very first step (see
        `cli/src/discover.rs`) and returns
        `DiscoverWriteError::PendingOpExists` regardless of
        the file's content -- only existence matters. The
        `pending-op.json` fixture is preferred over a
        pre-seeded `pool.json` because the
        `Missing` / `ValidUuidKeyed` / `Corrupt` classifier
        states all either no-op (`Missing`), short-circuit
        before any state-read trigger (`ValidUuidKeyed`
        errors in the bare `!args.write` branch but also
        errors in `write_discovered_membership` -- harder to
        attribute), or are the documented rebuild path
        (`Corrupt`, which proceeds with sidecar handling).
        A pending-journal fixture isolates the "did
        `write_discovered_membership` run before the lock"
        question to a single, unambiguous diagnostic.
     2. Acquire the external pool lock via the same
        `flock` holder used by the other subtests.
     3. Run `braid discover --write --expect-count 1`.

     Subtest assertions:
     - `rc != 0`.
     - output contains `"another braid operation is already in
       progress"` (the `AlreadyHeld` `Display`).
     - output does NOT contain `"discover refusing to write
       pool.json: pending-op.json exists at"` (the
       `PendingOpExists` error prefix at
       `cli/src/discover.rs:170-173`). If the invariant
       regressed so that `write_discovered_membership` ran
       before the lock acquire, its journal check would fire
       and surface that prefix through `print_cli_error` at
       `cli/src/main.rs:828`.
     - output does NOT contain `"no braid-labeled LUKS devices
       found"` (the device-scan empty-result diagnostic at
       `cli/src/main.rs:812`). If the invariant regressed so
       that `discover_pool_members` (the probe step) ran
       before the lock acquire, this diagnostic would fire on
       a test VM with no braid-labeled disks attached.
     - Cleanup between this subtest and the next: `rm -f
       /var/lib/braid/pending-op.json` so the journal does not
       bleed into other subtests. Run `braid status` (or
       similar read-only check) to confirm the file is gone.

     Together these two NOT-contain assertions cover both the
     membership-read regression and the probe regression for
     `discover --write` -- the two state-reads the dispatch
     arm actually performs. A regression that moved either
     before the lock acquire would surface one of the two
     diagnostics, so a test pass proves both stayed
     post-acquire.

  **Bounded-wait `ack` arm.** `ack` uses
  `acquire_with_timeout(Duration::from_secs(10))` per the per-
  command Acquisition-mode table; its assertion shape differs
  from the fail-fast arms.

  10. `braid --config /nonexistent/braid.json ack`
      -- holds the external pool lock for at least 11 s so the
      bounded wait expires inside the test window. Assertion:
      `rc != 0`, output contains `"another braid operation is
      already in progress"` (same `AlreadyHeld` `Display` text
      as fail-fast arms; ack's bounded wait converts to the same
      contention message on expiry), output does NOT contain any
      config-load / state-probe diagnostic, wall-clock elapsed
      is between 9 s and 14 s (matches
      `tests/module/alert-state-lock.py:239-243`'s pinned
      window so behavior stays consistent across the two tests).

  **Silent `monitor` arm.** `monitor` uses non-blocking `acquire`
  and exits 0 silently on contention per the per-command table.
  The lock-before-state-read invariant for monitor is observed
  as exit-0-without-touching-broken-input.

  11. `braid --config /nonexistent/braid.json monitor`
      -- with the external pool lock held. Assertion: `rc == 0`,
      stdout AND stderr contain neither `"No such file or
      directory"`, nor `"Configuration file not found"`, nor any
      pool-state diagnostic, nor any error-level log line. If
      monitor regressed to "load config, then acquire lock," the
      bad config path would trigger a setup error (exit 2 per
      `cli/src/main.rs`'s monitor doc-comment classification),
      not a silent exit 0.

  The test verifies the order of operations end-to-end on the
  real binary, not a mocked framework. Each subtest is run with
  a fresh external lock holder so the timing windows do not
  bleed (notably for ack's 10 s bounded wait, where a stale
  holder from the previous subtest could let ack succeed instead
  of expiring).

  Register as `pool-lock-precedes-state-read` in `flake.nix`
  `checksFor`. Run via `just test-vm
  pool-lock-precedes-state-read`.
- `braid-lock-then-unlock-no-race.py` (+ `.nix`) -- regression gate
  for the F2 finding (post-lock stop must be synchronous). Steps:
  (1) `braid unlock` until the pool is mounted and
  `braid-online.service` is `active`. (2) `braid lock` -- the
  command must return only after `braid-online.service` reaches
  `inactive`; assert `systemctl is-active braid-online.service`
  returns `inactive` immediately after `braid lock` exits. (3)
  Immediately invoke `braid unlock` again; mount the pool and
  assert it remains mounted for several seconds (no late ExecStop
  fires to re-lock it). Failure mode if the code regresses to
  `--no-block`: step 3's mount is unmounted by the late ExecStop
  and the assertion fails.
- `braid-lock-coordinator-race.py` (+ `.nix`) -- regression gate
  for the third-round F1 finding (stop coordinator must dissolve
  the three-way deadlock when an external `systemctl stop`
  overlaps a slow plain `braid lock`). Steps:
  (1) `braid unlock`. Confirm pool mounted, `braid-online`
  active.
  (2) Write a large urandom payload to slow down the next lock
  (so plain `braid lock`'s `cmd_lock` takes ~5-10 s of wall-clock
  unmounting and closing LUKS mappers under load).
  (3) Start plain `braid lock` in the background and capture its
  PID. Wait until it has acquired the stop coordinator (`flock -n
  /run/braid-stop-coordinator.lock true` fails) -- this proves
  plain holds the coordinator but the test runner has not yet
  observed `done\n`.
  (4) From another shell, invoke `systemctl stop
  braid-online.service` (synchronous). This is the external race.
  ExecStop reentry fires while plain is still mid-`cmd_lock`. The
  reentry's poll observes the coordinator held with empty content,
  then transitions to observing `done\n` once plain writes it.
  (5) Assert: `systemctl stop` returns success well before
  `TimeoutStopSec`. The wall-clock time from (4) to (5) is bounded
  by the remainder of plain's `cmd_lock` (a few seconds), NOT by
  the `lockSystemdStopDeadlineSecs` deadline.
  (6) Assert: plain `braid lock`'s exit code is 0; pool is
  unmounted; `braid-online.service` is `inactive`.
  Failure mode if the code regresses to a blocking-flock
  reentry: step (5) hangs until the deadline expires and the
  test fails by timeout.
- `mark-online-skips-start-while-deactivating.py` (+ `.nix`) --
  regression gate for the `Deactivating` snapshot rule
  end-to-end (the deadlock scenario walked in the plan's
  "Snapshot rule (deadlock avoidance)" section). The unit-test
  matrix proves `mark_online` skips the start when handed a
  `Deactivating` snapshot, but only an end-to-end test proves
  the snapshot call site actually sits between pool-lock
  acquisition and the mutation -- and that a future refactor
  cannot move it without failing this gate.

  Setup: a test-only NixOS drop-in replaces
  `braid-online.service`'s `ExecStop=` with a deterministic
  sleep so the unit stays in `deactivating` long enough for a
  concurrent mutator to snapshot it -- and so the real
  `braid lock --systemd-stop` never runs during the test (it
  would acquire the pool lock and unmount the pool, defeating
  the whole setup). Concretely, write
  `/run/systemd/system/braid-online.service.d/99-delay-stop.conf`
  with the leading `ExecStop=` clear so the real command list
  is reset, then a single sleep:

  ```ini
  [Service]
  ExecStop=
  ExecStop=/bin/sleep 15
  ```

  The empty `ExecStop=` resets the list per
  `reference/systemd/man/systemd.service.xml:394` (the same
  multi-command + empty-reset scheme as `ExecStart=`), and
  drop-ins are merged after the main unit per
  `reference/systemd/man/systemd.unit.xml:203`. Without the
  leading empty assignment systemd would *append* the sleep
  after the real `ExecStop=braid lock --systemd-stop ...`,
  which would (a) run `cmd_lock` under the held pool lock,
  unmounting the pool before `braid add` can run, and (b)
  contradict the assertion below that the stop is not a
  `braid lock`. The leading `ExecStop=` clear is therefore
  load-bearing.

  After the drop-in is written, `systemctl daemon-reload`. The
  15 s delay is large enough to cover the mutator's snapshot
  read without depending on walltime races; teardown removes
  the drop-in and reloads.

  Steps:
  (1) `braid unlock`; confirm pool mounted and `braid-online`
  active.
  (2) Write enough data to `/mnt/storage` that a subsequent
  `braid add` will run for a few seconds (so the test has a
  comfortable window to observe states).
  (3) Start `systemctl stop braid-online.service` in the
  background; immediately poll `systemctl show -P ActiveState
  braid-online.service` until it returns `deactivating`. The
  background stop is held there by the 15 s ExecStop delay.
  Crucially the stop is NOT a `braid lock` -- it does not
  acquire the pool lock; it only blocks systemd's stop
  transition for `braid-online.service`. This avoids the
  reviewer's anti-pattern of holding the pool lock externally
  (which would make the mutator in step 4 fail-fast before
  `mark_online` runs).
  (4) Run `braid add disk2=/dev/disk/by-id/virtio-disk2 ...`
  with a passphrase on stdin. Dispatch acquires the pool lock
  (uncontended -- the stop transition holds no pool lock),
  `snapshot` runs and observes `online_state =
  Deactivating`, the mutation runs to completion under the
  guard, and `mark_online` reaches its start gate.
  (5) Assert:
    - `braid add` exits 0.
    - The journal does NOT contain a `Starting
      braid-online.service...` line between the `braid add`
      start timestamp and the `braid add` exit timestamp -- if
      `mark_online` ran a `systemctl start` it would queue
      behind the in-flight stop and the journal would record
      the start request. Use
      `journalctl -u braid-online.service --since=... --until=...
      -o cat` and grep.
    - The background `systemctl stop` returns 0 well under
      `TimeoutStopSec` -- roughly after the 15 s sleep
      completes, plus the few tens of ms of systemd transition
      overhead. The load-bearing claim is "returns 0 without
      hanging into the stop deadline," not "exactly 15 s"; do
      not encode an exact upper bound that would make the test
      flaky against scheduler noise.
    - `systemctl show -P ActiveState braid-online.service`
      returns `inactive` after step (5)'s stop completes.
    - `mountpoint -q /mnt/storage` is true -- `braid add`'s
      mutation outcome (the pool is still mounted because
      `braid add` never asked for an offline-tear-down). The
      `mark_online` start-skip on `Deactivating` is the load-
      bearing outcome under test: a mounted pool with an
      `Inactive` `braid-online.service` afterwards is the
      expected (and acceptable) state -- the operator can
      run `systemctl start braid-online.service` to re-arm the
      shutdown hook.
  (6) Cleanup: remove
  `/run/systemd/system/braid-online.service.d/99-delay-stop.conf`,
  `systemctl daemon-reload`, run `braid lock` to drop the
  pool offline cleanly.

  Failure modes covered:
    - Snapshot moved earlier (before lock acquire): mutator
      would race the stop transition and the journal would
      contain a queued `Starting braid-online.service...`
      line.
    - Snapshot moved later (after the mutation, after the
      stop completes): snapshot would see `inactive` and
      `mark_online` would issue `start`, which would not
      queue (the stop is gone), so the journal WOULD contain
      a `Starting...` line. The test catches both regressions.
    - Snapshot rule changed to "start unconditionally": same
      journal regression catches it.

  Register as `mark-online-skips-start-while-deactivating`
  in `flake.nix` `checksFor`. Run via `just test-vm
  mark-online-skips-start-while-deactivating`.
- `execstop-cleans-stale-online.py` (+ `.nix`) -- regression gate
  for the F2-#2 finding (`--systemd-stop` reentry must do real
  cleanup when invoked against a stale `braid-online.service` +
  out-of-band unmount). Steps:
  (1) `braid unlock`, confirm pool mounted and `braid-online`
  active.
  (2) Simulate out-of-band unmount: `umount /mnt/storage`
  directly. The mappers under `/dev/mapper/braid-*` are still
  open; `braid-online.service` is still `active`
  (`ConditionPathIsMountPoint` only affects activation, not state
  while already active).
  (3) Record `ls /dev/mapper/braid-*` (expect 1 or more entries).
  Confirm no `flock` is held on
  `/run/braid-stop-coordinator.lock` (`flock -n
  /run/braid-stop-coordinator.lock true` succeeds), proving no
  plain `braid lock` is in flight.
  (4) `systemctl stop braid-online.service`. The unit's ExecStop
  fires `braid lock --systemd-stop`; the stop-coordinator probe
  finds the coordinator available (no `Held`), so the reentry
  acquires the coordinator + the pool lock and runs full
  `cmd_lock`.
  (5) Assert `ls /dev/mapper/braid-*` is empty -- orphan mappers
  were closed.
  (6) Assert `systemctl is-active braid-online.service` returns
  `inactive`.
  Failure mode if the code regresses to a mount-only fast-path:
  step 5's mappers are still open and the test fails.

Eval tests:

Both eval-test files are written as **functions imported from
`flake.nix`** (parameterized over `{ linuxPkgs, nixpkgs, linuxSystem }`)
rather than self-contained scripts -- this matches the pattern of
the existing `tests/*.nix` files referenced from `checksFor` in
`flake.nix:108-114` and lets the file reach the repo-root module
at `../../modules/braid` without ambiguity. **A relative path
like `./modules/braid` from inside `tests/eval/*.nix` would
resolve to `tests/eval/modules/braid` and silently break the
test.**

Three system-related constraints, all of which the harness must
satisfy together:

1. **The NixOS module must be evaluated against `linuxSystem`,
   not the host system.** `checksFor` already computes
   `linuxSystem = builtins.replaceStrings [ "-darwin" ] [ "-linux" ] system`
   (`flake.nix:104`) so that VM tests built from `aarch64-darwin`
   target `aarch64-linux`. The eval tests must do the same; a
   hardcoded `x86_64-linux` would silently target the wrong
   architecture on aarch hosts. The plan threads `linuxSystem`
   through as an explicit parameter.
2. **The stub `braid.package` must come from `linuxPkgs`**, not
   the host's `pkgs`. The module's existing assertion at
   `modules/braid/options.nix:83` requires `braid.package` when
   `braid.enable = true`; a `writeShellScriptBin` built against
   host pkgs would have the wrong target system embedded in its
   derivation outputs and could fail to evaluate cleanly when
   combined with a Linux-targeted nixosSystem.
3. **`braid.enable` must be `true`.** The assertion lives inside
   the existing `config = lib.mkIf cfg.enable { assertions = [
   ... ]; }` block in `modules/braid/options.nix:79-83`; without
   enabling, the assertion list is empty and the negative check
   would spuriously fail with "no assertion with the expected
   message found."

Shared harness (factored into a helper to keep the two files
diff-free except for the deadline value):

```nix
# tests/eval/_braid-eval-harness.nix
{ linuxPkgs, nixpkgs, linuxSystem, lockSystemdStopDeadlineSecs }:
nixpkgs.lib.nixosSystem {
  system = linuxSystem;
  modules = [
    ../../modules/braid
    {
      braid.enable = true;
      braid.package = linuxPkgs.writeShellScriptBin "braid" "exit 0";
      braid.lockSystemdStopDeadlineSecs = lockSystemdStopDeadlineSecs;
      # Provide other module options the existing braid asserts may
      # require; verify against `modules/braid/options.nix` at impl
      # time. The mountPoint default is "/mnt/storage" so no override
      # is needed there.
    }
  ];
}
```

- `tests/eval/lock-systemd-stop-deadline-assertion.nix` -- positive
  case. Returns a derivation that succeeds when the configuration
  with `lockSystemdStopDeadlineSecs = 270` evaluates cleanly (i.e.
  the toplevel build does not throw). This proves the option is
  wired into the module and that realistic configurations build.

  ```nix
  # tests/eval/lock-systemd-stop-deadline-assertion.nix
  { pkgs, linuxPkgs, nixpkgs, linuxSystem }:
  let
    good = import ./_braid-eval-harness.nix {
      inherit linuxPkgs nixpkgs linuxSystem;
      lockSystemdStopDeadlineSecs = 270;
    };
  in pkgs.runCommand "eval-lock-systemd-stop-deadline-ok" {
    # Force evaluation by depending on the toplevel. Building the
    # Linux toplevel from a Darwin host requires nix.linux-builder
    # to be enabled, which is the same dependency the existing VM
    # tests already declare in the project's nix-darwin config.
    inherit (good.config.system.build) toplevel;
  } "echo ok; touch $out"
  ```

  The wrapper `runCommand` uses host `pkgs` (so the wrapper
  derivation builds on the check system, matching the rest of
  `checksFor`), while the NixOS toplevel under test is the
  Linux-targeted evaluation.

- `tests/eval/lock-systemd-stop-deadline-assertion-fails.nix` --
  negative case. **NOT** a `builtins.tryEval`-based test (`tryEval`
  only exposes `{ success, value }` and silently swallows the
  assertion message; it cannot assert the message text). Instead,
  build the NixOS module evaluation WITHOUT triggering the
  toplevel `lib.assertMsg` chain by reading `config.assertions`
  directly (assertions are pure data on the evaluated module):

  ```nix
  # tests/eval/lock-systemd-stop-deadline-assertion-fails.nix
  { pkgs, linuxPkgs, nixpkgs, linuxSystem }:
  let
    # Import the same internal constant the module uses for the
    # unit's TimeoutStopSec so the test does not hardcode a
    # second copy of `300`. If the timeout is ever changed, the
    # module and this test move together.
    braidOnlineStopTimeoutSecs =
      (import ../../modules/braid/constants.nix).braidOnlineStopTimeoutSecs;
    # Drive the assertion to fire by setting the deadline equal to
    # the timeout (the predicate is `< braidOnlineStopTimeoutSecs`,
    # so equality fails it).
    bad = import ./_braid-eval-harness.nix {
      inherit linuxPkgs nixpkgs linuxSystem;
      lockSystemdStopDeadlineSecs = braidOnlineStopTimeoutSecs;
    };
    # MUST be a single-line Nix string (`"..."`, not `''...''`)
    # to match the assertion's literal in modules/braid/options.nix
    # byte-for-byte. A `''...''` literal here would prepend a
    # leading newline and the byte comparison would fail. The two
    # interpolations both feed from the constant above so the
    # string stays in sync if the timeout changes.
    expectedMessage = "braid.lockSystemdStopDeadlineSecs (${toString braidOnlineStopTimeoutSecs}) must be strictly less than braid-online.service TimeoutStopSec (${toString braidOnlineStopTimeoutSecs}).";
    matching = builtins.filter
      (a: a.message == expectedMessage)
      bad.config.assertions;
    ours = if matching == [] then null else builtins.head matching;
  in pkgs.runCommand "eval-lock-systemd-stop-deadline-fails" {} ''
    ${if ours == null then ''
        echo "no assertion with the expected message found" >&2
        echo "all assertions:" >&2
        ${nixpkgs.lib.concatMapStrings (a: "echo ${nixpkgs.lib.escapeShellArg "  - ${a.message}"} >&2\n") bad.config.assertions}
        exit 1
      '' else if ours.assertion then ''
        echo "assertion is true (passed) -- expected false (fail) for the equality case" >&2
        exit 1
      '' else ''
        echo ok
        touch $out
      ''}
  ''
  ```

  (If the implementer chooses to keep `braidOnlineStopTimeoutSecs`
  in `modules/braid/options.nix` instead of a sibling
  `constants.nix`, expose it via `_module.args` or a
  `lib.options.literalExpression`-style attr so the eval test can
  import it without re-importing the whole module. The
  single-source rule is what matters; the exact factoring is
  the implementer's call.)

  This treats `config.assertions` as data and asserts both that
  (a) an entry with the exact expected message exists, and
  (b) its `assertion` field is `false` (the rule would fire). The
  `runCommand` produces a derivation that succeeds only when both
  conditions hold. The wrapper uses host `pkgs` for the same
  reason as the positive case; the NixOS module is evaluated
  against `linuxSystem` via the shared harness.

  Registration in `flake.nix`:

  ```nix
  checksFor =
    system:
    let
      pkgs = nixpkgs.legacyPackages.${system};
      linuxSystem = builtins.replaceStrings [ "-darwin" ] [ "-linux" ] system;
      linuxPkgs = nixpkgs.legacyPackages.${linuxSystem};
      linuxCrane = craneFor linuxSystem;
    in
    {
      # ...existing entries...
      eval-lock-systemd-stop-deadline-ok =
        import ./tests/eval/lock-systemd-stop-deadline-assertion.nix {
          inherit pkgs linuxPkgs nixpkgs linuxSystem;
        };
      eval-lock-systemd-stop-deadline-fails =
        import ./tests/eval/lock-systemd-stop-deadline-assertion-fails.nix {
          inherit pkgs linuxPkgs nixpkgs linuxSystem;
        };
    };
  ```

  The `linuxPkgs` and `linuxSystem` additions to the existing
  `let` block in `checksFor` are NEW; the rest of the block is
  today's code (see `flake.nix:99-106`). On `aarch64-darwin` the
  eval checks therefore evaluate against `aarch64-linux`; on
  `x86_64-linux` (CI) they evaluate against the same system; the
  hardcoded-`x86_64-linux` regression is impossible because no
  string literal is involved.

**Both must be registered as flake checks** in
`flake.nix`'s `checksFor` (under `eval-lock-systemd-stop-deadline-ok`
and `eval-lock-systemd-stop-deadline-fails`) so they run under
`just test-vm` (which uses `nix build .#checks.<system>.<name>`).
Without registration the test files would sit in the tree without
being exercised; the new assertion could regress silently.

Existing tests that must continue to pass without change:

- `tests/module/pool-lock-contention.py` -- `unlock` fail-fast.
- `tests/module/pool-lock-replace-contention.py` -- `replace`
  fail-fast.
- `tests/module/alert-state-lock.py` -- in particular
  `tests/module/alert-state-lock.py:217-249` pins `ack`'s 10 s
  bounded wait (`elapsed >= 9 && elapsed <= 14`, retry message)
  and is the source-of-truth assertion for the
  `acquire_with_timeout` API contract.
- `tests/module/systemd-lifecycle.py`.
- `tests/module/lock-stops-bound-consumers.py` -- end-to-end
  cross-check that `cmd_lock`'s new BoundBy pre-step (which now
  goes through `OnlineStateOps::list_bound_by` and the typed
  `SystemctlShowBoundBy` `CmdRequest`) still stops every BoundBy
  consumer before umount. Failure mode if the typed seam regresses
  to skipping the consumer-stop: umount fails EBUSY and the test
  fails. Registered in `flake.nix:836-837`. *Assertions* stay
  unchanged across this migration; comments are refreshed
  separately (see "Tests to revise" below).

The contention message asserted by these tests is the source of
truth that `PoolLockError::AlreadyHeld`'s `Display` must match
verbatim.

Tests to revise:

- `tests/module/pool-lock-discover-contention.py` -- currently
  asserts that BOTH `braid discover --write` AND bare
  `braid discover` fail-fast on contention. The new model only
  locks `--write`. Revise the second subtest so bare
  `braid discover` is allowed to proceed under a held external lock
  (it never writes `pool.json`), with the assertion changed to
  "bare `discover` did NOT modify `pool.json`" (compare
  `cat /var/lib/braid/pool.json` before and after; bytes must
  match). The `--write` subtest is unchanged. Update the test's
  Intent/Why comments to match. Refresh Principle 12 prose if it
  implies bare `discover` is locked.
- `tests/module/lock-stops-bound-consumers.py` -- comments-only
  revise. The behavior assertions still apply (BoundBy consumer
  is stopped before umount), but the existing preamble and inline
  comments at lines 9, 11, 63, 75, 95, 97 attribute the BoundBy
  iteration to the wrapper (`"wrapper depends on this"`,
  `"wrapper's BoundBy loop stopped it"`, `"through
  braid-wrapper.sh's pre-stop block on reentry"`). Rewrite those
  comments to attribute the BoundBy iteration to `cmd_lock`'s
  pre-step (citing `cli/src/lock.rs` and `OnlineStateOps::
  list_bound_by` instead of the wrapper). DO NOT change any
  assertion, subtest structure, or test wiring -- the gate
  catches the same regression class either way, and a behavior
  edit here would create a churn liability that isn't justified
  by the migration's actual scope. Cross-listed under "Existing
  tests that must continue to pass without change" above (the
  *assertions* don't change) and here (the *comments* do).

Tests to replace (the wrapper-owned versions are obsolete once
fd 9 lives in Rust, not the wrapper bash):

- `tests/module/wrapper-pool-lock-not-inherited.py` -- the current
  test asserts the wrapper bash holds fd 9 and that no descendant
  (including `systemd-inhibit`) inherits it. After step 7 the
  wrapper bash never opens fd 9 at all. Replace with
  `tests/module/braid-pool-lock-not-inherited.py` (+ `.nix`) that
  asserts: (a) the braid binary itself is the lock holder; (b) the
  only `/proc/*/fd/*` entry symlinked to `/run/braid-pool.lock` is
  owned by the braid binary's PID, not any descendant
  (`systemd-inhibit`, `sh`, `sleep`, `cryptsetup`, `btrfs`); (c) on
  successful exit, no fd remains. The `O_CLOEXEC` open flag is the
  Rust-side invariant under test.
- `tests/module/wrapper-pool-lock-released-after-sigkill.py` --
  same redirection: replace with
  `tests/module/braid-pool-lock-released-after-sigkill.py` that
  SIGKILLs the braid binary mid-`add`, asserts no fd remains on
  `/run/braid-pool.lock` within a few seconds (kernel releases the
  flock on `OwnedFd` drop / process death), and a subsequent
  `flock -n /run/braid-pool.lock true` succeeds. The Rust-side
  invariant under test is that `RealPoolLockGuard`'s `Drop` (or
  process-exit fd cleanup) releases the kernel flock, and that
  `O_CLOEXEC` prevented any descendant from inheriting and pinning
  the open file description.

Update the corresponding `.nix` test entries in `flake.nix`'s
`checks` set when renaming.

## Docs

- `docs/principles.md:67` -- Principle 12: add `lock` and `enroll` to
  the locked-command list. Replace "in the wrapper" with "in
  `cli/src/main.rs` dispatch."
- `docs/decisions/018-systemd-lifecycle.md`:
  - §"CLI wrapper as synchronization layer" -- rewrite to "wrapper is
    a pure exec shim; synchronization lives in Rust dispatch."
    Cross-link to ADR 026.
  - §"Pool lock mutual exclusion" -- update line range references;
    move the "acquired in the wrapper" sentence to "acquired in
    `cli/src/main.rs` dispatch."
  - §"ExecStop bounded-wait pattern" -- promote from aspirational
    to current; cite the `--systemd-stop --deadline-secs` flag and
    the `lockSystemdStopDeadlineSecs` module option.
  - §"`systemctl start/stop` inside held-resource windows"
    (`docs/decisions/018-systemd-lifecycle.md:156`) -- amend rules
    2 and 3 to reflect the new architecture:
    - Rule 2 (`systemctl start`) stays as-is: `mark_online` keeps
      the snapshot-gated start (`Inactive` / `Failed` only) so the
      `Deactivating` deadlock cannot recur.
    - Rule 3 (`systemctl stop`) gets a documented carve-out for
      plain `braid lock`'s `mark_offline`: stop is now
      unconditional and synchronous, because the stop coordinator
      (`/run/braid-stop-coordinator.lock`) plus the `done\n`
      poll-out protocol guarantees the recursive ExecStop
      reentry exits 0 the moment plain finishes `cmd_lock` --
      never queuing a job behind the in-flight stop. The original
      rule 3 prevented a recursive-stop deadlock by gating on the
      snapshot; the coordinator-and-`done` protocol prevents the
      same deadlock by a different mechanism, and the snapshot
      is therefore not required on the stop side. Write this
      carve-out into the ADR as an explicit "Exception" sub-bullet
      under rule 3 so future readers don't reintroduce the
      snapshot gate and silently re-break post-lock synchrony
      (decision 018:131).
    - Add a cross-link to ADR 026 §"Stop coordinator + done
      protocol" so readers landing on rule 3 can follow to the
      mechanism that replaces the snapshot gate.
- `docs/decisions/026-pool-lock-rust-owned.md` (new, status
  `Active`): records the structural decision. Numbered 026 because
  021-025 are already taken (see "Files / New"). Sections:
  - Context (wrapper-CLI drift, missing lock+enroll coverage).
  - Decision (Rust-owned acquisition at dispatch; --systemd-stop
    flag; snapshot rule).
  - §"Snapshot rule on `systemctl start`" -- spells out the
    `Inactive` / `Failed` gate and the deadlock scenario from the
    plan's "Snapshot rule (deadlock avoidance)" section. This is
    the section ADR 018 rule 2 cross-links to.
  - §"Stop coordinator + done protocol" -- spells out the
    `/run/braid-stop-coordinator.lock` flock + `done\n` poll-out
    protocol that lets plain `braid lock`'s `mark_offline` run
    an unconditional synchronous `systemctl stop` without the
    recursive-stop deadlock. Explicitly states that this protocol
    is the mechanism that replaces ADR 018 rule 3's snapshot gate
    on the stop side. This is the section ADR 018 rule 3
    cross-links to.
  - Consequences (wrapper is pure exec; single source of truth for
    contention line; the snapshot rule is required on the start
    side to avoid the `Deactivating` deadlock; the coordinator is
    required on the stop side to replace the snapshot rule for
    `mark_offline`).
  - Cross-link to 018 and 019.
- `docs/index.md` -- add a one-line entry under `decisions/` for
  `026-pool-lock-rust-owned.md` (status `Active`, brief summary).

## Migration order

**Hard invariant: no command is ever locked by both the wrapper AND
Rust dispatch in the same released state.** The wrapper holds its
flock on fd 9 in the parent bash process; if the Rust child then
opens a new fd to the same lock file and attempts an exclusive
`flock`, the kernel returns `EWOULDBLOCK` (per-open-file-description
locks against the wrapper's own parent). Every locked command would
self-deadlock at dispatch and fail-fast with the contention message
against itself. The transitions below remove the wrapper case for
command X in the **same change** that adds Rust dispatch acquisition
for X.

**Third hard invariant: the wrapper's `9>&-` close-on-exec stays
in place through every transitional step.** The wrapper's
`@braidBin@ "$@" 9>&-` line at `modules/braid/braid-wrapper.sh:141`
drops the pool-lock fd in the forked child before exec, so braid
(and any descendant it spawns -- notably the long-lived
`systemd-inhibit` subprocess in `cli/src/inhibit.rs`, which is in
its own pgroup and can outlive braid) does not inherit fd 9.
During migration steps 4-7 the wrapper still opens fd 9 for the
still-wrapper-locked commands (`remove` / `remove-missing` /
`replace` / `discover` after step 4; `ack` / `monitor` until
step 6; `enroll` / `lock` until step 7). The temptation to
"clean up `9>&-` early" because the wrapper is shrinking must
be resisted: removing `9>&-` while any command still has its
wrapper flock case would let the inhibitor child inherit fd 9
and pin the pool lock past braid's exit, breaking the
`SIGKILL`-releases-flock invariant the matching VM tests
(`braid-pool-lock-not-inherited.py`,
`braid-pool-lock-released-after-sigkill.py`) verify. The
`9>&-` line is removed only in step 8, when the wrapper is
reduced to PATH + exec and no flock case remains.

**Second hard invariant: the pool lock is held through post-success
lifecycle work.** Decision 018:140 -- "The lock is held through
post-processing (permissions, `braid-online` activation)." If a
migration step adds Rust-side lock acquisition for command X without
also moving X's post-success lifecycle work (chown/chmod / `systemctl
start` for unlock/add/recover; `systemctl stop` for lock) into the
Rust guard at the same time, then between the Rust dispatch arm
dropping its guard and the wrapper's post-CLI fixup running, a
concurrent braid command can interpose. Post-success lifecycle work
moves into Rust **in the same step** that adds Rust-side lock
acquisition for the corresponding command.

Order (each numbered step is one or more commits; the whole sequence
can ship as one PR if preferred):

1. **Add `nix` crate + `cli/src/pool_lock.rs` module + unit tests.**
   Register the new module via `pub mod pool_lock;` in
   `cli/src/lib.rs`. No dispatch wiring yet. Just lands the seam.
   Verify `just test-rust`.
2. **Add `cli/src/online_state.rs` module + new `CmdRequest::
   SystemctlStart`, `SystemctlStop`, and `SystemctlShowBoundBy`
   variants + unit tests.** Register the new module via `pub mod
   online_state;` in `cli/src/lib.rs`. No dispatch wiring yet.
   (`SystemctlShowActiveState` is already present in
   `cli/src/cmd.rs:319` and is reused as-is.)
3. **Move `storage_group` to runtime config.** Add field to
   `Config`; extend `cli.nix` JSON. No behavior change yet (wrapper
   still does chown/chmod via its own substitutions).
4. **Atomic handoff for `unlock` / `add` / `recover`** (commands
   that produce a mounted pool and need `mark_online`). In the same
   commit:
   - Remove the wrapper's flock case for these three commands.
   - Remove the wrapper's post-CLI lifecycle case for these three
     (`braid-wrapper.sh:144-161` -- the `unlock|add|recover` arm of
     the `case "$subcmd" in ... esac` block that runs
     `mountpoint -q` / `chown` / `chmod` / `systemctl start
     braid-online.service`).
   - Add Rust dispatch acquisition + `snapshot` + post-success
     `mark_online` for these three commands in `cli/src/main.rs`,
     all inside the held-guard scope.
   - Verify: `pool-lock-contention.py` (covers `unlock`),
     `alert-state-lock.py` (still passes because `ack`/`monitor` are
     still wrapper-locked in this step), `systemd-lifecycle.py`,
     plus a manual smoke that `unlock`/`add`/`recover` still set
     `root:storageGroup 2770` on the mount and that
     `braid-online.service` activates.
   - The pool lock is held through `mark_online`. Decision 018:140
     is honored at every committed state.
5. **Atomic handoff for `remove` / `remove-missing` / `replace`**
   (mutators with no post-success lifecycle work). In the same
   commit, remove their wrapper flock case and add Rust dispatch
   acquisition. Verify `pool-lock-replace-contention.py`.
6. **Atomic handoff for `discover --write` / `ack` / `monitor`** (no
   post-success lifecycle work). In the same commit:
   - Remove their wrapper flock cases.
   - Add Rust dispatch acquisition per the per-command table
     (`discover --write`: `acquire`; bare `discover`: no acquire;
     `ack`: `acquire_with_timeout(10 s)`; `monitor`: `acquire`,
     silent exit 0).
   - Revise `tests/module/pool-lock-discover-contention.py` so the
     bare-`discover` subtest no longer asserts contention failure
     and instead asserts `pool.json` is byte-for-byte unchanged
     after a bare `discover` under a held external lock.
   - Replace `tests/module/wrapper-pool-lock-not-inherited.py` and
     `tests/module/wrapper-pool-lock-released-after-sigkill.py`
     with their Rust-owned equivalents
     (`braid-pool-lock-not-inherited.py`,
     `braid-pool-lock-released-after-sigkill.py`). The wrapper no
     longer holds fd 9, so the old tests are obsolete.
   - Update `flake.nix` `checks` to drop the obsolete names and add
     the new ones.
7. **Atomic handoff for `lock` (two-mode) and `enroll`, AND
   simultaneous ExecStop / NixOS-option switchover.** All of the
   following land in the same commit. The single-commit boundary
   is load-bearing: between adding Rust dispatch for plain
   `braid lock` (which calls synchronous `mark_offline`) and
   switching `braid-online.service`'s `ExecStop=` to the new
   flag form, any released tree would have systemd invoke
   `BRAID_SYSTEMD_EXECSTOP=1 braid lock` (no `--systemd-stop`
   flag), which the new Rust dispatch treats as the plain user-
   facing mode. That re-introduces the exact recursive-stop
   deadlock this plan removes: plain `braid lock` (as ExecStop)
   would call `systemctl stop braid-online.service` while inside
   `braid-online.service`'s own stop transition. There must be
   no commit boundary between these two changes.

   Concrete contents of the commit:

   - **Rust dispatch for `enroll` and `lock` (two-mode).** Add
     `pool_lock.acquire()` for `enroll` and plain `lock`; add
     `pool_lock.acquire_with_systemd_stop_deadline(...)` for
     `lock --systemd-stop --deadline-secs`. `cmd_lock` inlines
     the scrub-stop and BoundBy consumer-stop pre-steps (with
     the asymmetric warning semantics).
   - **Stop coordinator.** Add the
     `/run/braid-stop-coordinator.lock` flock and `done\n`
     content protocol. Plain `braid lock` acquires the
     coordinator FIRST (before the pool lock) and writes `done\n`
     after `cmd_lock` succeeds; the `--systemd-stop` ExecStop
     reentry probes it first (also before the pool lock) and
     either acquires-and-runs-`cmd_lock` (when uncontended) or
     polls for `done\n` / coord-release (when held).
   - **`mark_offline` moved to Rust.** Plain `braid lock`'s post-
     CLI synchronous `systemctl stop braid-online.service` lives
     in `online_state::mark_offline`, called under the pool
     guard. The `--systemd-stop` arm explicitly does NOT call
     `mark_offline`.
   - **NixOS option + eval-time assertion.** Add
     `braid.lockSystemdStopDeadlineSecs` to
     `modules/braid/options.nix` (default 270) with the
     single-line eval-time assertion that it is strictly less
     than the shared `braidOnlineStopTimeoutSecs` constant
     (see "NixOS module option + assertion" above). The same
     constant feeds the unit's `TimeoutStopSec=` in
     `modules/braid/storage.nix:144`; both sites must be wired
     up to the constant in this step so the literal `300` never
     appears twice in the released tree.
   - **ExecStop switchover.** Update
     `modules/braid/storage.nix:141` to
     `ExecStop = "${braidWrapped}/bin/braid lock --systemd-stop
     --deadline-secs ${toString cfg.lockSystemdStopDeadlineSecs}"`.
     Drop the `BRAID_SYSTEMD_EXECSTOP=1` environment prefix from
     the unit definition.
   - **Wrapper cleanup tied to this step.** Remove the
     wrapper's `lock`-pre-CLI scrub-stop and BoundBy-stop cases
     (`braid-wrapper.sh:88-132`), the `lock`-post-CLI
     `systemctl stop braid-online.service` case
     (`braid-wrapper.sh:162-167`), and the
     `BRAID_SYSTEMD_EXECSTOP` parse block
     (`braid-wrapper.sh:31-34`). With the unit no longer setting
     the env var and Rust no longer keying on it, the wrapper
     parse is dead code at the same instant.
   - **Eval-test registration.** Register
     `tests/eval/lock-systemd-stop-deadline-assertion.nix` and
     `tests/eval/lock-systemd-stop-deadline-assertion-fails.nix`
     as flake checks in `flake.nix`'s `checksFor`.
   - **Verify in this commit (every gate below must pass before
     the commit is merged):**
     - `braid-lock-systemd-stop.py` (both happy-path and
       deadline-expiry subtests),
     - `braid-lock-then-unlock-no-race.py` (synchronous post-
       lock stop regression gate),
     - `braid-lock-coordinator-race.py` (external `systemctl
       stop` racing a slow plain `braid lock`; this is the
       direct regression gate for the migration-order deadlock
       this consolidation prevents),
     - `execstop-cleans-stale-online.py` (stale-online + out-
       of-band unmount),
     - `pool-lock-lock-contention.py`,
     - `pool-lock-enroll-contention.py`,
     - `eval-lock-systemd-stop-deadline-{ok,fails}`,
     - all earlier-step VM tests still pass.
8. **Reduce wrapper to PATH + exec.** Drop now-unused
   `wrapper.nix` substitutions (`flockBin`, `mountpointBin`,
   `chownBin`, `chmodBin`, `systemctlBin`, `mountPointPath`,
   `storageGroup`). The wrapper is the three-line form documented
   in the Goal section. Verify all contention tests still pass.
9. **Update docs.** Principle 12, decision 018, new decision 026,
   plus a new `decisions/026-pool-lock-rust-owned.md` entry in
   `docs/index.md`.

After every numbered step, the tree is shippable: both invariants
(no double-locking; lock held through post-processing for every
migrated command) hold at the commit boundary, because each step
either (a) leaves the wrapper as authoritative for both the lock and
the post-success work of a command, or (b) moves both into Rust
together. There is no intermediate state where Rust holds the lock
for command X while the wrapper still owns X's post-success work.

## Verification

End-to-end checks before declaring done:

1. **All existing tests pass (with the replacements/revisions
   above).**
   - `just test-vm` -- the full VM test suite, including the
     unchanged `pool-lock-contention.py`,
     `pool-lock-replace-contention.py`, `alert-state-lock.py`,
     `systemd-lifecycle.py`; the revised
     `pool-lock-discover-contention.py` (bare `discover` no longer
     fails on contention); and the replacement
     `braid-pool-lock-not-inherited.py` and
     `braid-pool-lock-released-after-sigkill.py`. The old
     `wrapper-pool-lock-*` tests no longer exist.
   - `just test-rust` -- new `pool_lock.rs` + `online_state.rs` unit
     tests, new `LockArgs` clap tests.
   - `just test-parsers` -- unchanged parser coverage.

2. **New VM tests pass.**
   - `just test-vm braid-lock-systemd-stop` -- happy path AND deadline
     path.
   - `just test-vm pool-lock-enroll-contention`.
   - `just test-vm pool-lock-lock-contention`.
   - `just test-vm braid-pool-lock-not-inherited` -- Rust-side
     replacement for the wrapper test.
   - `just test-vm braid-pool-lock-released-after-sigkill` -- Rust-
     side replacement for the wrapper test.
   - `just test-vm braid-lock-then-unlock-no-race` -- F2
     regression gate (synchronous post-lock stop).
   - `just test-vm execstop-cleans-stale-online` -- F2-#2
     regression gate (stale-online + out-of-band unmount: ExecStop
     reentry must close orphan mappers).
   - `just test-vm pool-lock-precedes-state-read` -- mandatory
     invariant gate: lock acquire happens before config /
     membership / journal / probe / prompt I/O for every locked
     command.
   - `just test-vm braid-lock-coordinator-race` -- third-round F1
     regression gate: external `systemctl stop` overlapping a slow
     plain `braid lock` must not deadlock the unit until the
     deadline.
   - `just test-vm mark-online-skips-start-while-deactivating`
     -- snapshot-rule end-to-end gate: a mutator that runs while
     `braid-online.service` is `Deactivating` must skip the
     post-success `systemctl start` (journal contains no
     `Starting braid-online.service...` queued behind the
     in-flight stop).

3. **Eval-assertion flake checks pass.**
   - `just test-vm eval-lock-systemd-stop-deadline-ok`
     (equivalently `nix build
     .#checks.<system>.eval-lock-systemd-stop-deadline-ok`)
     -- positive case: `braid.lockSystemdStopDeadlineSecs = 270`
     evaluates without error.
   - `just test-vm eval-lock-systemd-stop-deadline-fails`
     -- negative case inspects `config.assertions` as data (see
     "Eval tests" above). The check derivation succeeds only when
     an assertion exists with `assertion = false` AND its
     `message` byte-for-byte equals the single-line string formed
     by interpolating the shared `braidOnlineStopTimeoutSecs`
     constant on both sides of "must be strictly less than" --
     same literal as in `modules/braid/options.nix`, kept in sync
     by sharing the constant rather than by duplicating the
     numeric value.

4. **Manual smoke (in a VM):**
   - `braid unlock` -> pool mounted, `braid-online.service` active.
   - `braid enroll <keyfile>` against a long-running `braid add` --
     fails fast with the canonical contention line.
   - `braid lock` against a long-running `braid add` -- fails fast.
   - `systemctl stop braid-online.service` against a long-running
     `braid add` -- waits, then unmounts cleanly after `add`
     finishes. Total elapsed time matches `add` duration plus a
     small acquisition delay, NOT `TimeoutStopSec`.
   - Override `braid.lockSystemdStopDeadlineSecs = 5`; hold the lock
     with a manual `flock`; `systemctl stop braid-online.service` --
     unit exits failed before `TimeoutStopSec`; journal contains
     `DeadlineExpired`.
   - Verify wrapper shrinkage: `wc -l modules/braid/braid-wrapper.sh`
     is ~3 lines; `grep flock modules/braid/braid-wrapper.sh` is
     empty.

5. **Doc check:**
   - `docs/principles.md:67` mentions `lock` and `enroll`.
   - `docs/decisions/018-systemd-lifecycle.md` cross-links to ADR
     026.
   - `docs/decisions/026-pool-lock-rust-owned.md` exists, status
     `Active`.
   - `docs/index.md` lists `026-pool-lock-rust-owned.md` under
     `decisions/`.
