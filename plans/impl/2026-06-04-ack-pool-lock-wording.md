# Fix stale pool-lock contention wording in `docs/commands/ack.md`

## Context

`braid ack` is the only command mapped to `LockPolicy::Timeout(Duration::from_secs(10))`
(`cli/src/main.rs#lock_policy`, the `Ack => Timeout(...)` arm). On contention it
polls `/run/braid-pool.lock` for up to 10 seconds (`cli/src/pool_lock.rs#RealPoolLock::acquire_with_timeout`):
if the holder releases within the window ack proceeds normally, otherwise it
exits 1 rendering `PoolLockError::AlreadyHeld`
(`braid: another braid operation is already in progress (pool lock /run/braid-pool.lock is held); retry once it finishes`).

The "Safety checks" bullet at `docs/commands/ack.md:58` was copied from the
fail-fast boilerplate the other nine mutating commands use (added in
`bc6ef909 docs(ack): document pool-lock contention`). It reads "Refuses if
another braid operation is in progress ... retry once it finishes" -- the
non-blocking wording, which is wrong for ack's bounded wait. An operator
reading only this page would not expect the ~10 s pause, and would not know
ack can succeed if the competing operation releases mid-wait.

The authoritative docs already describe ack correctly -- `docs/design/principles.md`
Principle 12 ("the 10-second alert acknowledgement window") and ADR 018
("`ack` waits up to 10 seconds before returning a retry message"). This change
only brings the user-facing command page into line with them; nothing else.

## Scope (confirmed)

- **Single edit, single file.** ack is the unique `Timeout`-policy command. The
  nine sibling pages are all **fail-fast** on pool-lock contention, so their
  identical "Refuses ..." bullet is **correct and stays as-is** -- but the
  mechanism is not uniform: `add`, `remove`, `remove-missing`, `replace`,
  `unlock`, `recover`, `enroll`, and `discover --write` use the `NonBlocking`
  dispatch policy, while plain `braid lock` is the special `LockPlain` path
  (`acquire_per_policy` does no dispatch-level acquire for it; `run_plain_lock`
  takes the stop coordinator, then the pool lock fail-fast via
  `acquire_pool_or_exit`). Either way the user-facing "Refuses ... retry once it
  finishes" wording holds for all nine.
- **No other surface repeats the claim.** Verified clean: `README.md`,
  `docs/index.md`, `docs/guides/monitoring-and-alerts.md`, `docs/commands/monitor.md`.
- **No change to "What happens under the hood."** Lock acquisition is a dispatch
  precondition, orthogonal to ack's operational narrative; sibling pages keep
  contention behavior in "Safety checks" only. Match that convention.
- **No design-doc cross-link added.** Command pages state lock behavior inline
  and self-contained; do not diverge.
- **No code or test change.** Behavior is already pinned by
  `tests/module/alert-state-lock.py` -- the "ack waits then fails without
  mutating alert state" subtest asserts `elapsed >= 9 && <= 14`, rc 1, and the
  retry message; the "ack re-acquires promptly when holder releases mid-wait"
  subtest pins the success path. The doc is being corrected to match
  already-tested, already-authoritative behavior.

## The edit

File: `docs/commands/ack.md` (line 58, the last "Safety checks" bullet).

Replace this exact line:

```
- Refuses if another braid operation is in progress (pool lock `/run/braid-pool.lock` is held) -- retry once it finishes.
```

with (single bullet line, matching sibling formatting):

```
- If another braid operation holds the pool lock (`/run/braid-pool.lock`), waits up to 10 seconds for it to finish: proceeds if the lock frees within that window, otherwise exits 1 with the pool-lock retry message.
```

Notes on the wording:

- States both outcomes (proceeds on mid-wait release; retry message on expiry),
  the two behaviors the tests pin.
- Refers to "the pool-lock retry message" by description rather than reproducing
  it, so the page cannot drift from the actual stderr. (Earlier drafts quoted a
  synthetic, non-verbatim string; the real message is quoted once in Context
  above and its source of truth is `cli/src/pool_lock.rs#PoolLockError::AlreadyHeld`.)
- ASCII only throughout, per repo CLI-output and writing-style conventions (no
  em-dash).

## Verification

1. `mdbook build docs` -- confirms the page renders and `mdbook-linkcheck2`
   passes (this edit adds no links, so linkcheck is unstressed; the build is a
   smoke test that the page is well-formed).
2. Read-back diff check: the new bullet says "waits up to 10 seconds" and names
   both outcomes; no remaining "Refuses" on that line.
3. Cross-doc consistency: the wording agrees with `docs/design/principles.md`
   Principle 12 and ADR 018's "ack waits up to 10 seconds" -- no edits needed to
   either; they are already correct.

No Rust tests, VM tests, or fixtures are affected.
