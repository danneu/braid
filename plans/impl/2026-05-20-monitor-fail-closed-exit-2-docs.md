# Plan: align monitor fail-closed + exit-2 docs across ADR 018, ADR 014, the manual, and CLI help

## Context

ADR 018 (`docs/decisions/018-systemd-lifecycle.md`) is the authoritative
record for the systemd lifecycle and is the first document a reviewer
opens before touching monitor/alert wiring. Line 95 still asserts:

> `braid monitor` exits 0 on operational errors after logging them, so
> health-polling failures do not directly start alert notification.

That claim contradicts the fail-closed contract that ADR 014 and the
code actually implement:

- `cli/src/monitor.rs:60-72,122-138` -- every `ProbeError` variant
  except `NotBtrfs` returns an `Err` that gets folded into an
  `AlertCause::ComputationError` cause, producing `MonitorResult::Alert`.
- `cli/src/main.rs` `Commands::Monitor` branch -- `MonitorResult::Alert(_)`
  -> `exit(1)`, `MonitorResult::Ok | PoolOffline` -> `exit(0)`.
- `modules/braid/monitor.nix:131-140` -- the systemd wrapper starts
  `braid-alert.service` on exit 1, so the headless surface beeps.
- `docs/decisions/014-alerts.md:69-78` -- the authoritative fail-closed
  contract: exit 0 = ok or offline, exit 1 = alert (including
  `ComputationError`), exit 2 = pre-`cmd_monitor` setup failure (any
  dispatch-layer fault before `cmd_monitor` is even called).
- `manual/guides/monitoring-and-alerts.md:21` -- already aligned with
  the post-fail-closed model ("When `braid monitor` detects an issue
  (exit code 1), the systemd wrapper starts `braid-alert.service`").

Commit timeline:
- `2ae1038` (2026-04-27) introduced the fail-closed code change.
- `55da2af` (2026-05-13) realigned the manual.
- `ff6f766` (2026-05-19, the pool-lock rewrite) edited ADR 018 line 95
  but only changed terminology -- the stale semantic claim survived
  the rewrite and is now the only surviving asserter of the old
  exit-0-on-errors model in the repo.

Risk #1 (the headline finding): a future implementer reads ADR 018:95
and ADR 014:69-78 back-to-back, sees two conflicting "decisions," and
reverts the fail-closed code to its old "log and exit 0" shape --
silently re-introducing the headless-surface beep gap that commit
`2ae1038` closed. Findings of this exact class have been raised in
code review; this plan dissolves them.

Risk #2 (the false-narrowing class): three other surfaces describe
exit 2 as "config unreadable" / "setup error (config)" only, even
though `Commands::Monitor` dispatch can also `exit(2)` on pool-lock
I/O failure (`pool_lock.acquire()` errors other than `AlreadyHeld`
flow through `handle_pool_lock_error` -> `exit(2)`, both in HEAD's
inlined `Commands::Monitor` branch and in the uncommitted
`acquire_per_policy` + `LockPolicy::MonitorSilent` path). A reader of
the deep-linked ADR 014:72 lands on the config-only parenthetical
right after the new ADR 018:95 bullet promises a broader category --
same kind of cross-doc contradiction this plan is meant to remove.
The three sibling surfaces are `docs/decisions/014-alerts.md:72`
(authoritative ADR), `manual/commands/monitor.md:29` (end-user exit
codes table), and the `Commands::Monitor` clap help comment at
`cli/src/main.rs:70` (rendered into `braid --help`). All three are
in-scope.

## Change

Coordinated documentation update across four files. Each edit converges
on the same precise exit-2 phrasing: `pre-cmd_monitor setup failure,
e.g. pool-lock I/O or config load failure`. No behavioral code or test
edits -- one of the four edits is a clap doc-comment on
`Commands::Monitor`, which renders as `braid --help` text but is not a
behavior change.

| # | File | Line | Surface |
|---|------|------|---------|
| 1 | `docs/decisions/018-systemd-lifecycle.md` | 95 | Systemd lifecycle ADR (headline finding) |
| 2 | `docs/decisions/014-alerts.md` | 72 | Authoritative alerts ADR (deep-link target) |
| 3 | `manual/commands/monitor.md` | 29 | End-user exit-codes table |
| 4 | `cli/src/main.rs` | 70 | `Commands::Monitor` clap help comment |

### Proposed replacement -- file 1 (`docs/decisions/018-systemd-lifecycle.md:95`)

Replace:

```
- `braid monitor` exits 0 on operational errors after logging them, so health-polling failures do not directly start alert notification.
```

With:

```
- `braid monitor` fails closed: probe/parse/stats/mountinfo failures, `acked-stats.json` baseline read/parse failures, and alert-latch read/quarantine failures latch `AlertCause::ComputationError` and exit 1, so the wrapper above starts the beeper. Exit 0 is reserved for healthy, pool-offline, and pool-lock-contended cycles; exit 2 is reserved for pre-`cmd_monitor` setup failures (e.g. pool-lock I/O, config load failure) and is never emitted by `cmd_monitor` itself. See [ADR 014 fail-closed contract](014-alerts.md#braid-monitor-is-a-pure-detector) for the cause taxonomy.
```

Rationale for each load-bearing clause:

- "fails closed" -- names the model explicitly so a reader scanning ADR
  18 catches the keyword and can grep ADR 014 for the full
  description.
- Cause list -- enumerates the precise read-side failures inside
  `cmd_monitor` that fold into `ComputationError`: subprocess (`Cmd`)
  and parse failures, `MountInfo` I/O, `load_acked_stats_fallible`
  (`cli/src/monitor.rs:90-91`), and `load_alert_latch_or_quarantine`
  (`cli/src/monitor.rs:131-132`). Read/quarantine-side only:
  `save_acked_stats` (`cli/src/monitor.rs:110-112`) and
  `save_alert_latch` (`cli/src/monitor.rs:144-148`) are best-effort
  -- their failure path is `eprintln!("Warning: ...")` and the cycle
  continues. Naming a bare "acked-stats/latch failures" bullet would
  over-narrow the doc to include writes the code does NOT fail closed
  on; the read/parse vs. read/quarantine qualifiers prevent that drift.
- "exit 1, so the wrapper above starts the beeper" -- explicitly
  re-affirms the line-94 contract under failure, closing the
  gap between line 94 and line 95.
- "Exit 0 is reserved for healthy, pool-offline, and pool-lock-contended
  cycles" -- captures the lock-contention exit-0 (already documented at
  ADR 018:146) and the offline exit-0 (ADR 014:78,84) so the bullet is
  self-contained.
- Exit 2 sentence -- names the two real pre-`cmd_monitor` exit-2
  sources in dispatch: pool-lock I/O failure (the `LockPolicy::MonitorSilent`
  branch in dispatch routes `pool_lock.acquire()` errors other than
  `AlreadyHeld` through `handle_pool_lock_error` -> `exit(2)`; in
  current HEAD this is inlined in `Commands::Monitor`, in the
  uncommitted dispatch refactor it lives in `acquire_per_policy`) and
  config load failure (`load_config_or_exit(..., 2)` for the Monitor
  branch). Pool-lock contention is exit 0, not exit 2 -- the
  `AlreadyHeld` short-circuit in the same branch goes to `exit(0)`,
  which is also why the new bullet lists "pool-lock-contended cycles"
  under exit 0. Writing "(config unreadable)" alone would falsely
  narrow exit 2 to one of its two sources; "(e.g. pool-lock I/O,
  config load failure)" is path-stable across the in-flight dispatch
  refactor and matches the actual surface area. The "never emitted by
  `cmd_monitor` itself" clause preserves the ADR 014:72 invariant for
  systemd wrapper authors reading ADR 018 in isolation.
- Deep link to ADR 014's `### braid monitor is a pure detector`
  subsection -- defers the cause taxonomy to the alerts ADR instead of
  duplicating it. Anchor `#braid-monitor-is-a-pure-detector` is
  mdbook-conformant.

Cross-reference style matches existing ADR 018 conventions (compare
line 171: `[ADR 026 snapshot rule](026-pool-lock-rust-owned.md#snapshot-rule-on-systemctl-start)`).

### Proposed replacement -- file 2 (`docs/decisions/014-alerts.md:72`)

This is the authoritative source the new ADR 018 line deep-links to.
The parenthetical must broaden to match.

Replace:

```
- **2** -- pre-monitor setup failure (config unreadable). Reserved for "could not even attempt to detect"; never emitted by `cmd_monitor` itself.
```

With:

```
- **2** -- pre-`cmd_monitor` setup failure (e.g. pool-lock I/O, config load failure). Reserved for "could not even attempt to detect"; never emitted by `cmd_monitor` itself.
```

Note: ADR 014:74 ("Fail closed: any failure inside `cmd_monitor` ...
Exit 2 means the monitor never ran") stays as-is -- it already frames
exit 2 as "monitor never ran," which is correct under both sources;
only the parenthetical example list was incomplete.

### Proposed replacement -- file 3 (`manual/commands/monitor.md:29`)

End-user exit-codes table. Match the ADR phrasing in user-facing
prose.

Replace:

```
| **2** | Pre-monitor setup error -- config unreadable |
```

With:

```
| **2** | Pre-monitor setup error (e.g. pool-lock I/O, config load failure) |
```

The surrounding rows (0/1) are correct and unchanged. The diagram at
`manual/guides/monitoring-and-alerts.md:154` reads "2 = setup error"
without a parenthetical and is already broad enough -- no edit.

### Proposed replacement -- file 4 (`cli/src/main.rs:70`)

Clap doc-comment on the `Monitor` variant -- rendered into
`braid --help` and `braid help monitor`. Single-line constraint
(clap's first-line help-string convention).

Replace:

```rust
    /// Check disk health: exit 0 = ok/offline, exit 1 = alert (incl. probe/compute failure latched as ComputationError), exit 2 = setup error (config)
    Monitor,
```

With:

```rust
    /// Check disk health: exit 0 = ok/offline, exit 1 = alert (incl. probe/compute failure latched as ComputationError), exit 2 = setup error (e.g. pool-lock I/O, config load)
    Monitor,
```

The line is dirty-tree-stable: `git diff cli/src/main.rs` does not
touch line 70 in the uncommitted dispatch refactor (verified via
`git diff cli/src/main.rs | grep Monitor` -- the only diff hunks
around `Monitor` are the new `LockPolicy::MonitorSilent` entries, not
the help comment). Editing this line does not collide with the
in-flight refactor.

## Critical files

- `docs/decisions/018-systemd-lifecycle.md` (1 line replaced, headline
  fix).
- `docs/decisions/014-alerts.md` (1 line replaced, authoritative
  alignment so the deep-link target is consistent).
- `manual/commands/monitor.md` (1 line replaced, end-user table).
- `cli/src/main.rs` (1 line replaced, clap help comment; mixes with
  the existing dirty tree but does not collide with any diff hunk).

No other surface asserts a config-only exit-2 contract -- verified by
`rg -n "config unreadable|setup error \(config\)|setup error -- config" docs/decisions manual/commands manual/guides cli/src`,
which returns exactly the three sibling lines above and nothing else
(see Verification step 4).

## Verification

This is a documentation-only change. No code paths or tests are
affected by the edit itself, but it is worth running the standing
fail-closed test pins to confirm the contract the new prose describes
is still the contract the code implements:

1. **Read the modified bullet alongside ADR 014:69-78.** Verify that
   every clause in the new line is consistent with the alerts ADR --
   especially the exit-0/1/2 split and the "never emitted by
   cmd_monitor" caveat for exit 2.
2. **Click-test the deep link.** Open the modified ADR 018 in mdbook
   (or just verify the anchor manually): `#braid-monitor-is-a-pure-detector`
   must resolve to the header `### \`braid monitor\` is a pure detector`
   at `docs/decisions/014-alerts.md:65`.
3. **Confirm no surviving "exits-0-on-errors" wording (Risk #1).** Run:
   ```
   rg -n "exits 0 on operational|operational errors|health-polling failures|exit-0-on-errors" docs/ manual/commands manual/guides modules/ cli/src
   ```
   The pre-edit run must return exactly one hit
   (`docs/decisions/018-systemd-lifecycle.md:95`); the post-edit run
   must return zero. The search is scoped to source markdown
   (`manual/commands`, `manual/guides`) -- it intentionally excludes
   `manual/book/` (generated mdbook HTML + searchindex.js) and the
   broad `monitor.*exit 0.*error` regex, which would otherwise match
   legitimate aligned content like the lifecycle diagram at
   `manual/guides/monitoring-and-alerts.md:154`
   (`-> braid monitor (exit 0 = ok, 1 = alert, 2 = setup error)`) and
   make a correct edit look failed. Verified pre-edit: this grep
   returns only the offending line.
4. **Confirm no surviving "config-only" exit-2 wording (Risk #2).** Run:
   ```
   rg -n "config unreadable|setup error \(config\)|setup error -- config" docs/decisions manual/commands manual/guides cli/src
   ```
   The pre-edit run must return exactly three hits:
   `docs/decisions/014-alerts.md:72`, `manual/commands/monitor.md:29`,
   and `cli/src/main.rs:70`. The post-edit run must return zero. This
   grep matches only stale config-only wording -- it does not match
   the new broadened "(e.g. pool-lock I/O, config load failure)"
   phrasing because the parenthetical token "(config" is followed by
   ` load` (not `)` or ` unreadable`). The scope omits `modules/`
   (no monitor exit-2 prose lives there) and `manual/book/`
   (generated). Verified pre-edit: returns exactly the three known
   stale lines.
5. **Confirm fail-closed contract is still pinned by tests.** Run
   `just test-rust` and verify these named tests still pass:
   - `cmd_monitor_latches_computation_error_on_mountinfo_io_failure`
     (`cli/src/monitor.rs:372`)
   - `cmd_monitor_corrupt_acked_stats_latches_computation_error`
     (`cli/src/monitor.rs:307`)
   - `cmd_monitor_corrupt_alert_latch_latches_computation_error`
     (`cli/src/monitor.rs:500`)
   - the `probe_pool_alerts` non-`NotBtrfs` error case at
     `cli/src/monitor.rs:332-354`

   These tests are the executable form of the new ADR 018:95 wording;
   if any of them fail, the prose update is premature.
6. **Confirm `git diff` shows exactly four single-line edits across
   the four critical files.** Three of the four files are clean before
   this plan starts (only `cli/src/main.rs` is dirty). `git diff
   --stat -- <each file>` should report `1 +-` for ADR 018, ADR 014,
   and `manual/commands/monitor.md`. For `cli/src/main.rs`, the
   diff already has the in-flight dispatch refactor; the new help-comment
   edit must appear as a separate one-line `-`/`+` pair around
   `Monitor,` and must not touch any other hunk. No accidental
   whitespace edits in any file.
7. **(Optional) Render `braid --help` and `braid help monitor`.**
   Confirm the new exit-2 wording shows up rendered (clap respects the
   doc-comment first line). If running the binary is inconvenient
   during the plan implementation, this can be skipped -- the help
   text is a verbatim render of the comment, so the edit verification
   above is sufficient.

## Out of scope

- Broader cause-taxonomy edits to ADR 014. Only the parenthetical on
  line 72 is touched; the fail-closed framing, the latch policy, the
  exit-code list, and every other section of ADR 014 stay as-is. This
  plan does not rewrite the alerts ADR -- it tightens one
  cross-referenced phrase so the deep-link landing matches the
  upstream wording.
- Any behavioral code change in `cmd_monitor`, the dispatch, or the
  wrapper. The `cli/src/main.rs:70` edit is a clap doc-comment only:
  it changes user-visible help text but does not alter command
  parsing, dispatch routing, exit codes, or any test outcome. The
  in-flight dispatch refactor (`LockPolicy::MonitorSilent`,
  `acquire_per_policy`) in the dirty working tree is separately
  reviewed and not part of this plan.
- The unrelated `monitor exits 0 silently on contention` claim on ADR
  018:146 -- that one is correct under the current model (lock
  contention is a deliberate skipped cycle, not a probe failure) and
  is reaffirmed by the new ADR 018:95 line's "pool-lock-contended
  cycles" clause.
- Regenerating `manual/book/` (the mdbook HTML output). That is
  produced by a separate build step (`mdbook build manual/`) and is
  expected to be regenerated on release; reflowing it here would
  inflate the diff with thousands of lines of generated content.
