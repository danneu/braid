# Plan: document the dual-path intent of `run_lock_pre_steps`

## Context

A review finding (Low / Simplicity) observed that in the `--systemd-stop`
(ExecStop) path, `run_lock_pre_steps` re-runs the scrub-stop + `BoundBy`
consumer-stop sequence that systemd's `BindsTo`+`After` cascade has already
performed before ExecStop fires. It proposed gating those pre-steps to the
plain-lock invocation via a new `systemd_stop` flag.

Verification (prior turn) confirmed the no-op claim *only for consumers that
follow the documented full triad* and rejected the *fix*: running the pre-steps
unconditionally is intentional, and the no-op is not universal. It keeps
teardown code-owned and uniform regardless of how systemd's cascade played out,
and is defense-in-depth for a consumer declared `BindsTo` without `After`: the
documented contract is the full `WantedBy`+`BindsTo`+`After` triad, but a
misordered consumer (BindsTo, no After) is not ordered before ExecStop, so the
blocking `systemctl stop` in `run_lock_pre_steps` is what would still free a
busy mount before unmount. Gating it off would narrow the EBUSY guard the
`lock-stops-bound-consumers` test exists to protect (Principle 1: resilient by
default) and would add a flag threaded through the public `cmd_lock` API --
more complexity, not less.

The remedy is documentation (no behavior change) plus one behavioral test that
locks in the defense-in-depth the docs assert (Change 4) -- without it, a future
regression that gates off the ExecStop pre-steps would pass silently, because
the existing test only covers full-triad consumers. Three confusions to dissolve:

1. `run_lock_pre_steps` (`cli/src/lock.rs:1134`) has no doc comment, so its
   dual-path role -- and why the ExecStop re-run is deliberate -- is invisible.
2. Decision 018's "On `lock`" step 3 says the `BoundBy` iteration is "for
   user-initiated `braid lock`," but the "On system shutdown" section
   (`018:143-148`) never mentions ExecStop re-runs the same pre-steps -- so the
   plain-lock framing reads as exclusive.
3. The finding leaned on decision 018's held-window `systemctl start` deadlock
   warning (`018:170`) to call this "risky," but that warning is about starting
   `braid-online.service` itself; `run_lock_pre_steps` stops *other* units
   (consumers/scrub), so no job queues against `braid-online.service` and the
   deadlock cannot apply. Nothing states that scope, so the misread is easy.

Intended outcome: a future reviewer auditing either the code or the ADR finds
the invariant stated, and does not have to re-derive that the ExecStop
redundancy is safe -- without changing any behavior.

## Changes

### 1. `cli/src/lock.rs` -- doc comment on `run_lock_pre_steps` (above line 1134)

Add a short `///` (the two helpers it calls, `stop_unit_silent` and
`stop_unit_warn_on_error`, already carry `///`, so this matches file style and
the project Doc Comments rule). Keep it to the shared-precondition statement
plus a pointer to decision 018 for the systemd ordering rationale -- do not
inline the full no-op-vs-load-bearing reasoning in code (that lives in the ADR,
the authority). Proposed wording (keep ASCII and `--`):

```rust
/// Shared pre-unmount teardown for both plain `braid lock` and the
/// `--systemd-stop` ExecStop path: stop scrub units, then each `BoundBy
/// braid-online.service` consumer. Run unconditionally so teardown is
/// code-owned regardless of systemd's cascade ordering; decision 018 covers
/// when these ExecStop stops are no-ops vs. load-bearing.
```

### 2. `docs/design/decisions/018-systemd-lifecycle.md` -- close the lock-vs-shutdown asymmetry

Edit the "On system shutdown" list (`018:143-148`). Append to item 1 (the
cascade) a note that ExecStop re-runs the same pre-steps, and state precisely
when those re-issued stops are no-ops versus load-bearing -- do not call them a
blanket no-op (that contradicts the defense-in-depth rationale). Reference the
"On `lock`" steps by their section position, not by raw line number. Proposed
item 1 replacement:

> 1. systemd stops `braid-online.service` (if active); its `BindsTo`+`After`
>    cascade stops the scrub units and any full-triad consumer first. ExecStop
>    then re-runs the same scrub-stop + `BoundBy` iteration as the "On `lock`"
>    steps 2-3. For the scrub units and any consumer that follows the documented
>    `WantedBy`+`BindsTo`+`After` triad, the cascade has already stopped them, so
>    these re-issued stops are no-ops. A consumer that declares `BindsTo` without
>    `After` has no stop-ordering guarantee and may still be active when ExecStop
>    runs, so the explicit blocking stop here is what frees the mount. Running the
>    pre-steps unconditionally covers both cases, keeping teardown code-owned and
>    independent of cascade ordering.

Keep this as plain prose (no new markdown cross-link) so `mdbook-linkcheck`
stays green. Optionally soften "On `lock`" step 3's trailing sentence ("This
mirrors the cascade...") only if it now reads redundantly against item 1; it is
accurate as-is, so leave it unless the implementer judges otherwise.

### 3. `docs/design/decisions/018-systemd-lifecycle.md` -- scope the held-window warning

In the "`systemctl start/stop` inside held-resource windows" Rules list
(`018:174-179`), add one scoping sentence so a reader does not over-apply the
warning. Proposed addition (new closing rule or sentence on the section intro):

> These rules govern `start`/`stop` of `braid-online.service` itself. The
> `systemctl stop` calls in `run_lock_pre_steps` target bound consumers and
> scrub units, not the lifecycle owner, so they queue no job against
> `braid-online.service` and the start-behind-stop deadlock above does not
> apply to them.

### 4. `tests/module/lock-stops-bound-consumers` -- prove the misordered-consumer guard

Why this is needed (and why the plan is no longer docs-only): the existing
fixture's `dummy-pool-consumer` uses the full triad (`wantedBy` + `bindsTo` +
`after`), so cycle 2 passes whether or not `run_lock_pre_steps` runs under
`--systemd-stop` -- the cascade stops that consumer before ExecStop either way.
It therefore does **not** prove the `BindsTo`-without-`After` defense the docs
now assert; a future regression that gates off the ExecStop pre-steps would stay
green. Add a cycle that fails if the guard is removed.

Fixture (`lock-stops-bound-consumers.nix`): add a second consumer unit
`dummy-pool-consumer-unordered.service` that:

- declares `bindsTo = [ "braid-online.service" ]` and
  `unitConfig.ConditionPathIsMountPoint = "/mnt/storage"`, but **no `after`**
  (and no `wantedBy`, so it does not auto-start during cycles 1-2 -- leaving
  those cycles and their assertions untouched; start it manually in the new
  cycle). Dropping `wantedBy` is a deliberate narrowing from the finding's
  "BindsTo/WantedBy": the property under test is the missing `After`, and
  keeping `wantedBy` would force it active in every cycle and rewrite cycles 1-2.
- holds an fd under `/mnt/storage` and is SIGTERM-resistant so it outlives
  braid's busy-umount retry window, which is only ~1s
  (`UMOUNT_RETRY_ATTEMPTS = 3`, `UMOUNT_RETRY_DELAY = 500ms`, `cli/src/lock.rs:17-18`).
  Trap SIGTERM and bound the stop with `TimeoutStopSec` well above that window
  (e.g. `10`s, comfortably below the lock deadline). Representative ExecStart:

  ```sh
  exec 3>/mnt/storage/.consumer-unordered-lock
  trap '' TERM
  while :; do sleep 1; done
  ```

  with `serviceConfig.TimeoutStopSec = 10;`.

Test (`lock-stops-bound-consumers.py`): add cycle 3 -- with the pool unlocked,
`systemctl start dummy-pool-consumer-unordered.service`, assert it is active and
its fd 3 resolves under `/mnt/storage` (reuse `assert_consumer_holds_mount`,
parameterized by unit name), confirm it appears in `BoundBy braid-online.service`,
then `systemctl stop braid-online.service` and assert clean teardown: consumer
inactive, `/mnt/storage` unmounted, both mappers gone. Determinism: without the
guard the consumer still holds the fd through the ~1s umount retries (it dies
only at the `TimeoutStopSec` SIGKILL), so umount fails and the mount survives
(red); with the guard, `run_lock_pre_steps`' blocking `systemctl stop` waits for
the consumer to die before unmount (green).

Update the Intent/Why/Scenario preamble in both the `.nix` and `.py` (project
Test Conventions) to name the misordered consumer and the guard cycle 3 proves.

## What NOT to change

- **Do not** add a `systemd_stop` flag or gate the pre-steps (the rejected
  finding fix). Behavior stays identical; Change 4 locks that in.
- **Do not** rewrite cycles 1-2 or the full-triad `dummy-pool-consumer`. They
  remain the proof of the no-op path (cascade stops the consumer before
  ExecStop); Change 4 is purely additive (a new unit + cycle 3).

## Verification

- `mdbook build docs` succeeds (mdbook-linkcheck validates no broken
  cross-links were introduced) -- AGENTS.md "Documentation".
- `cargo build -p braid-cli` (or `just test-rust`) compiles -- confirms the new
  `///` is well-formed and breaks nothing.
- `just test-vm lock-stops-bound-consumers` passes all three cycles (the new
  cycle 3 is the only added VM coverage; blast radius is one test).
- RED-without-guard sanity check (TDD): temporarily filter **only**
  `dummy-pool-consumer-unordered.service` out of the `BoundBy` loop in
  `run_lock_pre_steps` (add it to the skip `matches!` at `cli/src/lock.rs:1151-1156`),
  confirm cycle 3 fails (mount survives) while cycles 1-2 stay green, then revert.
  Do not skip the whole iteration -- cycle 1's plain `braid lock` relies on the
  same loop to stop the full-triad `dummy-pool-consumer` before unmount (no
  cascade in plain lock), so a blanket skip would redden cycle 1 too and blur
  the signal. Filtering the single unit isolates the cycle 3 guard.
- Re-read decision 018: "On `lock`" steps 2-3 and "On system shutdown" item 1
  now cross-reference each other; item 1 distinguishes the no-op (full-triad)
  case from the load-bearing (`BindsTo`-without-`After`) case; and the
  held-window section scopes its warning to `braid-online.service`.
