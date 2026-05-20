# Plan: drop stale "wrapper" references after ADR 026

## Context

ADR `docs/decisions/026-pool-lock-rust-owned.md` (status: Active) moved
`/run/braid-pool.lock` ownership from the shell wrapper into Rust
dispatch on 2026-05-19, and ADR 013 already put mount-point permission
fixups (`mark_online`) under Rust dispatch. The wrapper
(`modules/braid/braid-wrapper.sh`) is now a 3-line `PATH`-and-`exec`
shim and acquires no locks, runs no systemctl calls, and sets no
permissions.

The doc + comment sweep that should have accompanied those migrations
missed many sites across the manual, an ADR, Rust source comments,
and the NixOS VM test preambles. Operators reading any of them will
hunt for shell-wrapper behaviour that does not exist, and maintainers
following the same paths land on the wrong file.

This plan does one job: re-attribute ownership across every remaining
stale site to "Rust dispatch" / "`mark_online`" / "`mark_offline`" /
"`cmd_lock`" / "the braid CLI", matching how the ADRs already
describe the post-migration architecture. No source behaviour
changes.

## Audit

Identified by `grep -rn wrapper cli/src tests/module docs/decisions
manual/commands manual/guides`, then filtered against ADR 026, ADR
013, and `modules/braid/braid-wrapper.sh`.

### Stale -- fix in this plan

Each entry is "wrapper does X" where X is now Rust dispatch's job.

- `manual/commands/add.md:113`, `manual/commands/unlock.md:86`,
  `manual/commands/recover.md:86`, `manual/commands/remove.md:63`,
  `manual/commands/remove-missing.md:80`,
  `manual/commands/replace.md:116`, `manual/commands/discover.md:72`
  -- "(\`/run/braid-pool.lock\` is held by another wrapper)".
- `docs/decisions/014-alerts.md:76` -- "holds the wrapper-level pool
  lock before invoking the Rust CLI".
- `cli/src/unlock.rs:135` -- "wrapper holds the pool flock for the
  lifetime of unlock".
- `cli/src/doctor.rs:4406` -- "the wrapper has just started
  braid-online.service" (in the
  `braid_online_check_warns_when_activating` test prologue).
- `cli/src/pool_lock.rs:204` -- test name
  `already_held_display_is_wrapper_compatible_verbatim`. (Note: line
  numbers shifted from earlier draft -- error string is now
  `cli/src/pool_lock.rs:28-30`, test is `cli/src/pool_lock.rs:203-209`.)
- `tests/module/pool-lock-contention.nix:3,8` -- "wrapper's flock
  acquisition" / "if the wrapper regresses to a blocking flock".
- `tests/module/pool-lock-contention.py:3,60,72` -- "the wrapper must
  fail fast" / "the wrapper regresses to blocking flock" / "wrapper
  regressed to blocking flock".
- `tests/module/pool-lock-replace-contention.nix:3,7` -- "the wrapper
  pool lock" / "Without wrapper-level serialization".
- `tests/module/pool-lock-replace-contention.py:16` -- "fail at the
  wrapper lock".
- `tests/module/pool-lock-discover-contention.py:69` -- "wrapper lock
  check must not fire".
- `tests/module/alert-state-lock.nix:3,8` -- "acquires the wrapper's
  /run/braid-pool.lock" / "The wrapper lock is the serialization
  boundary".
- `tests/module/alert-state-lock.py:8` -- "wrapper-level
  serialization".
- `tests/module/systemd-lifecycle.nix:4,8` -- "CLI wrapper
  synchronization" / "A broken wrapper or misconfigured dependency".
- `tests/module/systemd-lifecycle.py:5,9,11,21,23,24-25,26-27,91,119,
  127,141,217,230,234,247,249,252,304,324,340,422` -- mix of stale
  ("wrapper's flock", "wrapper's post-lock systemctl stop", "wrapper
  prints warning", "wrapper's WARNING code path", "wrapper activation
  failure") and imprecise ("through the wrapper", "bypassing the
  wrapper", "wrapper didn't run"). Both kinds are rewritten.
- `tests/module/add-bootstrap.nix:5,8,9` -- "the wrapper sets mount
  point permissions" / "wrapper-based permission fixup" / "regression
  in the wrapper". `mark_online` (Rust) sets permissions per ADR 013.
- `tests/module/add-bootstrap.py:4,7,11` -- same claims about wrapper
  setting mount permissions.
- `tests/module/scrub-lifecycle.nix:16` -- "wrapper stops
  timer+service, CLI unmounts".
- `tests/module/scrub-lifecycle.py:6,21,257` -- "the wrapper stops the
  timer and service first" / "wrapper stops timer+service before CLI
  unmounts" / "The wrapper must stop the timer and service before CLI
  attempts unmount". ADR 026 explicitly puts this in the Rust lock
  path: "The lock path stops lifecycle-bound scrub units and BoundBy
  `braid-online.service` consumers before unmounting."

### Out of scope (verified legitimate; do not touch)

This is the **complete enumeration** of `wrapper` hits within the
verification scope (`cli/src docs/decisions manual/commands
manual/guides tests/module`) that should remain in the post-fix tree.
Verification step 3 cross-references this list.

**A. Shell wrapper's `PATH`-injection role** -- still its only job
per ADR 026.

- `cli/src/inhibit.rs:13`
- `cli/src/ups.rs:900,904`
- `cli/src/doctor.rs:4182,4183`
- `cli/src/tui/probe.rs:2788`
- `tests/module/ups-preflight-on-battery.py:13`
- `manual/guides/ups.md:150`
- `docs/decisions/010-toolchain-pinning.md:17`
- `docs/decisions/016-auto-suspend.md:83`
- `docs/decisions/020-ups-integration.md:55`

**B. Shell wrapper as historical/architectural reference** -- names
the file or records pre-026 state in context.

- `cli/src/lock.rs:1544` ("old wrapper" in test prologue)
- `cli/src/online_state.rs:3` ("used to live in the shell wrapper")
- `docs/decisions/013-mount-permissions.md:26`
- `docs/decisions/018-systemd-lifecycle.md:117,153,194,200`
- `docs/decisions/026-pool-lock-rust-owned.md:2,16,23,28,42,134`

**C. Systemd unit "wrapper"** -- shorthand for the systemd service
wrapping the binary's exit code. Project convention; user already
chose to leave these.

- `docs/decisions/014-alerts.md:67,74`
- `cli/src/monitor.rs:61`
- `manual/guides/monitoring-and-alerts.md:21`

**D. `braid-beep-probe` wrapper** -- a separate canonical
privilege-drop shell wrapper.

- `cli/src/cmd.rs:317`
- `cli/src/doctor.rs:8,947,1118,1135,3937,3946,3968,3993,4031,4034,4039,4078`
- `manual/commands/doctor.md:92`
- `tests/module/braid-alert.py:26,29,34,35,44,56,61,64,65,66`
- `docs/decisions/014-alerts.md:94`

**E. NixOS sendmail wrapper** -- smartd module concept.

- `tests/module/smartd-config.nix:4,13`
- `tests/module/smartd-config.py:4,7,10`

**F. Other system wrappers** -- NixOS-generated wrappers and the
braid UPS module wrapper (a NixOS option wrapper, not the shell
wrapper).

- `tests/module/fan-control.py:24`
- `tests/module/ups-credential-lifecycle.py:8`
- `docs/decisions/005-sane-defaults.md:13`

**G. Rust-internal function / type / test "wrappers"** -- code-level
naming, no relation to the shell wrapper.

- `cli/src/add.rs:7395`
- `cli/src/cmd.rs:1480`
- `cli/src/discover.rs:276`
- `cli/src/enroll_key_file.rs:1049`
- `cli/src/lock.rs:3544,3550,3602,3615`
- `cli/src/luks.rs:275,534,1255,2762,2764`
- `cli/src/main.rs:1135`
- `cli/src/membership.rs:71,447,602`
- `cli/src/mount.rs:358`
- `cli/src/mount_check.rs:648`
- `cli/src/parse/systemctl_list_units.rs:88`
- `cli/src/pool.rs:751,792,1481,1529`
- `cli/src/preview.rs:220`
- `cli/src/recover.rs:11593,11656`
- `cli/src/replace.rs:2033,2036,4469`
- `cli/src/status_tag.rs:252`
- `cli/src/test_fixtures/enroll_key_file.rs:72`
- `cli/src/test_fixtures/mount.rs:41`
- `cli/src/test_fixtures/unlock.rs:22`
- `cli/src/types.rs:247`
- `cli/src/util.rs:44`
- `docs/decisions/022-dry-run-preview-model.md:32,55`

**Outside verification scope** -- `modules/**`, `plans/**`, and
`reference/**` are not audited or rewritten by this plan. `plans/impl/`
in particular contains historical plan files that quote the stale
wording (e.g.
`plans/impl/2026-05-12-serialize-discover-pool-lock.md:69`,
`plans/impl/2026-05-15-split-pool-lock-pending-op-refusals.md:45,95`,
`plans/impl/2026-05-19-rust-owned-pool-operation-lock.md:571,1235`);
those are historical record, not active documentation, and the
verification greps must be scoped to exclude them.

## Changes

### 1. Seven manual pages

In each file, replace the single matching line

```
- Refuses if another braid operation is in progress (`/run/braid-pool.lock` is held by another wrapper) -- retry once it finishes.
```

with

```
- Refuses if another braid operation is in progress (pool lock `/run/braid-pool.lock` is held) -- retry once it finishes.
```

Files: `manual/commands/add.md:113`, `unlock.md:86`, `recover.md:86`,
`remove.md:63`, `remove-missing.md:80`, `replace.md:116`,
`discover.md:72`.

Rationale for "pool lock `/run/braid-pool.lock` is held": parallels
the CLI error's parenthetical (`PoolLockError::AlreadyHeld` at
`cli/src/pool_lock.rs:28-30`: `"(pool lock /run/braid-pool.lock is
held)"`) without claiming the manual is verbatim-pinned by the
existing Rust test. The manual line wraps the path in Markdown
backticks for rendering; the CLI error string does not. Treat the
manual grep and the CLI-string Rust unit test as **independent**
checks for their respective sources -- not as one transitively
covering the other.

### 2. `docs/decisions/014-alerts.md:76`

Current:

```
Every command that writes `acked-stats.json` or `alert-latch.json` (`monitor`, `ack`, `add`, `remove`, `remove-missing`) holds the wrapper-level pool lock before invoking the Rust CLI.
```

Replace with:

```
Every command that writes `acked-stats.json` or `alert-latch.json` (`monitor`, `ack`, `add`, `remove`, `remove-missing`) acquires `/run/braid-pool.lock` in Rust dispatch (see [ADR 026](026-pool-lock-rust-owned.md)) before reading state or running probes.
```

### 3. `cli/src/unlock.rs:135`

Replace the comment

```rust
// wrapper holds the pool flock for the lifetime of unlock.
```

with

```rust
// Rust dispatch holds the pool flock for the lifetime of unlock.
```

(Single-line change inside the existing multi-line comment.)

### 4. `cli/src/doctor.rs:4406`

In the test preamble for `braid_online_check_warns_when_activating`,
replace

```rust
* Scenario: the wrapper has just started braid-online.service and
```

with

```rust
* Scenario: `mark_online` has just started braid-online.service and
```

### 5. `cli/src/pool_lock.rs:204` -- rename test

Rename `already_held_display_is_wrapper_compatible_verbatim` to
`already_held_display_verbatim`. The body unchanged; the body still
asserts the full verbatim CLI string against the same hard-coded
literal -- the Rust test guards the CLI display only.

### 6. `tests/module/pool-lock-contention.nix:1-9`

Replace:

```nix
# Test: pool-lock-contention
#
# What: Verifies the wrapper's flock acquisition is non-blocking and
# fails fast when another process holds /run/braid-pool.lock.
#
# Why: Without -n on the flock call, a wedged holder would silently
# hang any concurrent `braid unlock` invocation forever. This test
# guards the failure layer — it must fail if the wrapper regresses
# to a blocking flock.
```

with:

```nix
# Test: pool-lock-contention
#
# What: Verifies Rust dispatch's flock acquisition is non-blocking and
# fails fast when another process holds /run/braid-pool.lock.
#
# Why: Without -n on the flock call, a wedged holder would silently
# hang any concurrent `braid unlock` invocation forever. This test
# guards the failure layer -- it must fail if Rust dispatch regresses
# to a blocking flock.
```

(Also replaces the em-dash with `--` per project style.)

### 7. `tests/module/pool-lock-contention.py`

At lines 3 and 60-62 and 72, change

- L3: "the wrapper / must fail fast (exit 1)" -> "Rust dispatch / must
  fail fast (exit 1)".
- L60: "the wrapper regresses to blocking flock." -> "Rust dispatch
  regresses to blocking flock."
- L72: "unlock hung past 5s wall-clock cap -- wrapper regressed to" ->
  "unlock hung past 5s wall-clock cap -- Rust dispatch regressed to".

### 8. `tests/module/alert-state-lock.nix:1-9`

Replace:

```nix
# Test: alert-state-lock
#
# What: Verifies that every alert-state mutator acquires the wrapper's
# /run/braid-pool.lock before it can mutate alert-latch.json or
# acked-stats.json.
#
# Why: monitor, ack, add, remove, and remove-missing all touch alert
# state. The wrapper lock is the serialization boundary that prevents
# stale read-modify-write cycles from resurrecting acknowledged alerts.
```

with:

```nix
# Test: alert-state-lock
#
# What: Verifies that every alert-state mutator acquires
# /run/braid-pool.lock in Rust dispatch before it can mutate
# alert-latch.json or acked-stats.json.
#
# Why: monitor, ack, add, remove, and remove-missing all touch alert
# state. The dispatch-level pool lock is the serialization boundary
# that prevents stale read-modify-write cycles from resurrecting
# acknowledged alerts.
```

### 9. `tests/module/alert-state-lock.py:8`

Change "Without wrapper-level serialization" to "Without dispatch-level
serialization".

### 10. `tests/module/pool-lock-replace-contention.nix:1-9`

Replace the "What:" and "Why:" preamble lines:

- L3: "Verifies that `braid replace` takes the wrapper pool lock before"
  -> "Verifies that `braid replace` acquires the pool lock in Rust
  dispatch before".
- L7: "Without wrapper-level serialization, concurrent" -> "Without
  dispatch-level serialization, concurrent".

### 11. `tests/module/pool-lock-replace-contention.py:16`

Change "fail at the wrapper lock and leave no pending-op.json" to
"fail at the pool lock and leave no pending-op.json".

### 12. `tests/module/pool-lock-discover-contention.py:69`

Change "wrapper lock check must not fire after release" to "pool lock
check must not fire after release".

### 13. `tests/module/systemd-lifecycle.nix:1-13`

In the preamble:

- L4: "and CLI wrapper" -> "and Rust dispatch's lifecycle
  synchronization".
- L8: "A broken wrapper or misconfigured" -> "A broken dispatch
  lifecycle path or misconfigured".

(Lines may shift; the change is conceptual -- the preamble is one
block. Update accompanying nearby text only if needed for grammar.)

### 14. `tests/module/systemd-lifecycle.py` -- broad preamble + inline cleanup

Single, mechanical re-attribution. In every comment in this file:

- "via CLI wrapper" -> "via the braid CLI" (L21, L23).
- "wrapper script that must stay synchronized" / "broken wrapper" ->
  "Rust dispatch's lifecycle code that must stay synchronized" /
  "broken dispatch" (L9, L11).
- "wrapper prints warning" / "Wrapper warns but succeeds" /
  "wrapper's WARNING code path" -> "`mark_online` prints warning" /
  "`mark_online` warns but succeeds" / "`mark_online`'s WARNING code
  path" (L24-25, L249, L252).
- "wrapper activation failure" -> "`mark_online` activation failure"
  (L247).
- "wrapper's non-blocking flock" / "wrapper's flock check" ->
  "Rust dispatch's non-blocking flock" / "Rust dispatch's flock check"
  (L26-27, L141, L340, L422).
- "wrapper's post-lock `systemctl stop braid-online`" ->
  "`mark_offline`'s post-unmount `systemctl stop braid-online`"
  (L91).
- "through the wrapper" -> "through the braid CLI" (L234, L304,
  L324).
- "bypassing the wrapper" -> "bypassing the braid CLI" (L217).
- "(wrapper didn't run)" -> "(`mark_online` didn't run)" (L230).
- "CLI wrapper synchronization" (subtest headers) -> "Rust dispatch
  synchronization" (L5, L119, L127).

The L21/L23 "via CLI wrapper" wording is technically still true (the
PATH shim is on the way to the binary) but reads as if the wrapper
owns the activation logic. Re-attribute for consistency with the
rest of the file.

### 15. `tests/module/add-bootstrap.nix:1-13` and `tests/module/add-bootstrap.py:1-11`

In each preamble:

- "the wrapper sets mount point permissions" -> "`mark_online` sets
  mount point permissions" (.nix L5, .py L4, .py L11).
- "wrapper-based permission fixup must cover" -> "`mark_online`'s
  permission fixup must cover" (.nix L8, .py L7).
- "regression in the wrapper" -> "regression in `mark_online`" (.nix
  L9).

`mark_online` lives at `cli/src/online_state.rs` per ADR 013.

### 16. `tests/module/scrub-lifecycle.nix:16` and `tests/module/scrub-lifecycle.py:6,21,257`

In each comment, replace "the wrapper stops the timer and service"
(or variants: "wrapper stops timer+service", "The wrapper must stop
the timer and service") with "the lock path stops the scrub timer
and service" -- the language ADR 026 uses verbatim. Specifically:

- `scrub-lifecycle.nix:16`: "wrapper stops timer+service, CLI
  unmounts." -> "the lock path stops timer+service, then CLI
  unmounts."
- `scrub-lifecycle.py:6`: "because the wrapper stops the timer and
  service first" -> "because the lock path stops the timer and
  service first".
- `scrub-lifecycle.py:21`: "wrapper stops timer+service before CLI
  unmounts." -> "the lock path stops timer+service before CLI
  unmounts."
- `scrub-lifecycle.py:257`: "The wrapper must stop the timer and
  service before CLI attempts unmount." -> "The lock path must stop
  the timer and service before CLI attempts unmount."

## Files modified

Manual:

- `manual/commands/add.md`
- `manual/commands/unlock.md`
- `manual/commands/recover.md`
- `manual/commands/remove.md`
- `manual/commands/remove-missing.md`
- `manual/commands/replace.md`
- `manual/commands/discover.md`

Decision records:

- `docs/decisions/014-alerts.md`

Rust source (comments / test name only -- no behaviour change):

- `cli/src/unlock.rs`
- `cli/src/doctor.rs`
- `cli/src/pool_lock.rs`

NixOS VM tests (comments only -- no test logic change):

- `tests/module/pool-lock-contention.nix`
- `tests/module/pool-lock-contention.py`
- `tests/module/alert-state-lock.nix`
- `tests/module/alert-state-lock.py`
- `tests/module/pool-lock-replace-contention.nix`
- `tests/module/pool-lock-replace-contention.py`
- `tests/module/pool-lock-discover-contention.py`
- `tests/module/systemd-lifecycle.nix`
- `tests/module/systemd-lifecycle.py`
- `tests/module/add-bootstrap.nix`
- `tests/module/add-bootstrap.py`
- `tests/module/scrub-lifecycle.nix`
- `tests/module/scrub-lifecycle.py`

No source behaviour changes. No nixpkgs/Cargo lock changes. No new
files. No tests added or removed.

## Verification

Independent checks -- the Rust unit test and the manual/test/comment
greps each cover their own source; one does not transitively cover
the other.

All greps below are **scoped to the active paths** (`cli/src`,
`docs/decisions`, `manual/commands`, `manual/guides`, `tests/module`)
to exclude historical plan files under `plans/impl/` (which legitimately
quote the stale wording, see "Out of scope") and `reference/` /
`modules/` (out of plan scope). Patterns use `-F` (fixed strings) and
single-quoted arguments so backticks in the search string never
trigger command substitution.

1. **Stale-phrase smoke greps return zero matches** (each spells the
   active pathspecs inline; do **not** factor them into a shell
   variable -- the project shell is zsh, which does not word-split
   unquoted scalar expansions, so `git grep ... -- $PATHS` would pass
   one pathspec containing spaces and silently match nothing). All
   patterns use `-F` (fixed strings) and single-quoted arguments so
   backticks/apostrophes inside the pattern are literal:

   ```
   git grep -n -F -- 'held by another wrapper'        -- cli/src docs/decisions manual/commands manual/guides tests/module
   git grep -n -F -- 'wrapper-level pool lock'        -- cli/src docs/decisions manual/commands manual/guides tests/module
   git grep -n -F -- 'wrapper-level serialization'    -- cli/src docs/decisions manual/commands manual/guides tests/module
   git grep -n -F -- 'wrapper_compatible'             -- cli/src docs/decisions manual/commands manual/guides tests/module
   git grep -n -F -- 'wrapper-based permission'       -- cli/src docs/decisions manual/commands manual/guides tests/module
   git grep -n -F -- 'wrapper lock check'             -- cli/src docs/decisions manual/commands manual/guides tests/module
   git grep -n -F -- 'wrapper pool lock'              -- cli/src docs/decisions manual/commands manual/guides tests/module
   git grep -n -F -- "wrapper's flock"                -- cli/src docs/decisions manual/commands manual/guides tests/module
   git grep -n -F -- "wrapper's WARNING"              -- cli/src docs/decisions manual/commands manual/guides tests/module
   git grep -n -F -- "wrapper's non-blocking"         -- cli/src docs/decisions manual/commands manual/guides tests/module
   git grep -n -F -- "wrapper's post-lock"            -- cli/src docs/decisions manual/commands manual/guides tests/module
   git grep -n -F -- 'wrapper stops the timer'        -- cli/src docs/decisions manual/commands manual/guides tests/module
   git grep -n -F -- 'wrapper stops timer'            -- cli/src docs/decisions manual/commands manual/guides tests/module
   git grep -n -F -- 'wrapper regresses'              -- cli/src docs/decisions manual/commands manual/guides tests/module
   git grep -n -F -- 'wrapper regressed'              -- cli/src docs/decisions manual/commands manual/guides tests/module
   git grep -n -F -- 'the wrapper sets'               -- cli/src docs/decisions manual/commands manual/guides tests/module
   git grep -n -F -- 'wrapper activation failure'     -- cli/src docs/decisions manual/commands manual/guides tests/module
   git grep -n -F -- 'CLI wrapper synchronization'    -- cli/src docs/decisions manual/commands manual/guides tests/module
   git grep -n -F -- 'via CLI wrapper'                -- cli/src docs/decisions manual/commands manual/guides tests/module
   git grep -n -F -- 'bypassing the wrapper'          -- cli/src docs/decisions manual/commands manual/guides tests/module
   git grep -n -F -- "wrapper didn't run"             -- cli/src docs/decisions manual/commands manual/guides tests/module
   git grep -n -F -- 'through the wrapper'            -- cli/src docs/decisions manual/commands manual/guides tests/module
   git grep -n -F -- 'broken wrapper'                 -- cli/src docs/decisions manual/commands manual/guides tests/module
   git grep -n -F -- 'wrapper prints'                 -- cli/src docs/decisions manual/commands manual/guides tests/module
   git grep -n -F -- 'Wrapper warns'                  -- cli/src docs/decisions manual/commands manual/guides tests/module
   git grep -n -F -- 'regression in the wrapper'      -- cli/src docs/decisions manual/commands manual/guides tests/module
   git grep -n -F -- 'wrapper must stop'              -- cli/src docs/decisions manual/commands manual/guides tests/module
   git grep -n -F -- 'at the wrapper lock'            -- cli/src docs/decisions manual/commands manual/guides tests/module
   ```

   Each should produce no output. Battery is precise rather than
   regex-based on purpose -- a broad regex picks up legitimate `cli/src`
   hits ("the wrapper fails fast when invoked" for `braid-beep-probe`,
   "the wrapper runner removes" for Rust test helpers, etc.).

   If a future reader wants to compress the repetition, the safe
   factoring under zsh is `set -A PATHS cli/src docs/decisions
   manual/commands manual/guides tests/module` followed by
   `... -- "${PATHS[@]}"` -- never an unquoted scalar.

2. **Manual parenthetical appears in exactly 7 pages** (single-quoted
   so backticks are literal):
   ```
   git grep -n -F -- 'pool lock `/run/braid-pool.lock` is held' -- manual/commands
   ```
   Should return 7 lines, one per file in section 1 of "Changes".

3. **Residual `wrapper` audit** -- compare the post-fix output to the
   enumerated allowlist in "Out of scope (verified legitimate)":
   ```
   git grep -nw wrapper -- cli/src docs/decisions manual/commands manual/guides tests/module
   ```
   The output must be **exactly** the union of categories A through G
   in the audit. Diff against the pre-fix output to confirm: the only
   removed/changed lines should be the stale entries listed in "Stale
   -- fix in this plan"; no other line numbers should disappear or
   gain a new "wrapper does X" attribution.

4. **Rust tests pass**: `just test-rust`, including the renamed
   `already_held_display_verbatim`. The Rust pinned-string test still
   guards the CLI display only; it does not (and cannot) catch manual
   drift.

5. **VM tests pass**: `just test-vm pool-lock-contention
   alert-state-lock pool-lock-replace-contention
   pool-lock-discover-contention systemd-lifecycle add-bootstrap
   scrub-lifecycle` -- only comments changed; test logic is untouched.
   Run individually if the full set is slow.

6. **Optional manual sanity**: `cd manual && mdbook build` and open
   `manual/book/commands/add.html` to confirm the updated line renders
   cleanly. Built artifacts under `manual/book/` are regenerated on
   build and are not part of this plan to commit.

## Implementation notes

- Commit `cd2fd12` already completed the planned test-comment sweep,
  `cli/src/unlock.rs` comment update, and pool-lock test rename before
  this implementation. This pass only changed the remaining active stale
  wrapper references in the manual, ADR 014, and `cli/src/doctor.rs`.
- The residual `wrapper` audit still includes
  `docs/decisions/018-systemd-lifecycle.md:95`, which is the same
  legitimate systemd service wrapper category as the ADR 014 monitor
  lines already allowlisted by the plan, so it remained unchanged.
