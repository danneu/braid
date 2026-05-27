# Plan: document pool-lock contention in `braid ack` command doc

## Context

A review finding flagged that `docs/commands/ack.md` never documents that
`braid ack` can fail purely due to pool-lock contention, even though ADR 014
and ADR 026 make acquiring `/run/braid-pool.lock` part of `ack`'s contract.

Investigation confirmed the finding and sharpened it into a **consistency
gap**, not a one-off omission:

- `ack` dispatches under `LockPolicy::Timeout(10s)` (`cli/src/main.rs:163`).
  On contention beyond 10s it returns `PoolLockError::AlreadyHeld`
  (`cli/src/pool_lock.rs:61,94`) -- message: `braid: another braid operation
  is already in progress (pool lock /run/braid-pool.lock is held); retry once
  it finishes` -- and exits 1 (`cli/src/main.rs:1052-1053`).
- ADR 014 (`docs/design/decisions/014-alerts.md:89`) names `ack` among the
  commands that acquire `/run/braid-pool.lock` in Rust dispatch.
- **Every other lock-taking mutator doc already carries an identical,
  verbatim safety-check line**, and `ack.md` is the lone omission:
  `add.md:115`, `remove.md:65`, `remove-missing.md:82`, `replace.md:117`,
  `unlock.md:92`, `lock.md:58`, `recover.md:86`, `enroll.md:73`.
  (`monitor.md` correctly omits it: `monitor` uses `MonitorSilent` and exits 0
  silently on contention, so it never refuses.)

Intended outcome: `ack.md` carries the same standard line as its siblings, so
the doc tree is uniform and an operator who sees `ack` fail during a concurrent
`add`/`remove`/`replace` has the documented explanation and remedy.

## Decision: use the verbatim convention line (not bespoke wording)

`ack` is unique in using `Timeout(10s)` rather than `NonBlocking`, but the
10s wait is a deliberate anti-noise mechanism (`cli/src/pool_lock.rs:55-56`:
bounded wait so brief `monitor` contention does not produce noisy immediate
failures), designed to be invisible in the common case. It changes the
mechanism, not the user-facing contract: on sustained contention `ack` still
refuses, and the remedy is still "retry once it finishes" -- identical to the
siblings.

Therefore reuse the exact sibling line rather than the finding's bespoke
"10s timeout / past the deadline" phrasing. Rejected alternatives:

- **Note the 10s wait in the line.** More precise, but implies a delay the
  operator essentially never sees and cannot act on, and breaks the verbatim
  uniformity of the 8-doc convention. Mechanism rationale already lives in the
  `pool_lock.rs` comment and ADR 014, which is the right home for it.
- **Extract a shared "pool lock" concept doc and link all command docs to it.**
  Larger refactor against a clear, low-cost, intentional convention (one
  greppable line per doc). Out of scope for this finding.
- **Also add it to `ack.md`'s "What happens under the hood" section.** Siblings
  do not double-document; that section describes the alert-cleanup flow, not
  lock acquisition. Single bullet under "Safety checks" matches convention.

## Change

**File:** `docs/commands/ack.md`

Under the existing `## Safety checks` section (currently three bullets, lines
53-57), append one bullet, verbatim from the sibling docs:

```
- Refuses if another braid operation is in progress (pool lock `/run/braid-pool.lock` is held) -- retry once it finishes.
```

This is the only change. No code, no other docs, no cross-links added.

## Verification

1. **Consistency check** -- the line should now appear in `ack.md` and match
   the siblings byte-for-byte. After the edit:

   ```
   rg -n "Refuses if another braid operation is in progress" docs/commands/
   ```

   Expect 9 matches (the 8 existing siblings plus `ack.md`), all identical.

2. **Docs build** -- confirm the mdBook tree still builds and linkcheck passes
   (per `docs/book.toml`, a broken cross-link fails the build):

   ```
   mdbook build docs
   ```

   The new bullet adds only an inline `code` span, no cross-links, so this is a
   smoke check that nothing in `ack.md` broke.

No Rust or VM tests are relevant -- this is end-user reference prose only.
