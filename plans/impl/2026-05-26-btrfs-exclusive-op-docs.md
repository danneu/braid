# Fix the "refuses on any btrfs exclusive operation" doc inaccuracy

## Context

The four mutating-command reference docs claim braid **refuses** when any btrfs
exclusive operation is in flight. The code does not. The shared mutation
preflight `check_exclusive_op_with_policy(..., RejectPausedBalanceElseEnqueue)`
(`cli/src/preflight.rs:196-201`) hard-refuses **only a paused balance**; for any
other in-flight op (running balance, device add/remove/replace, resize, swap
activate) it returns `Ok(Some(op))`, which `require_mutation_preflight`
(`preflight.rs:599-619`) surfaces as a `PreviewNote::Info("waiting for in-flight
{op} to finish...")` and proceeds -- braid issues its `btrfs` commands with
`--enqueue` and lets the kernel serialize.

So the docs invert a safety-relevant fact: an operator reading the refusal list
expects `remove`/`add`/`replace`/`remove-missing` to bounce while a
balance/device-add/replace is running, when they actually queue and wait. The
behavior is already pinned by tests (`mutation_preflight_rejects_balance_paused`
at `preflight.rs:1688`, `mutation_preflight_busy_op_returns_info_note` at
`preflight.rs:1705`) -- this change only brings the prose in line with
already-tested behavior. No code or test changes.

`docs/commands/lock.md:36` is **correct** (lock uses `RejectAnyBusy` via
`require_lock_preflight`, `preflight.rs:627-629`) and must be left untouched.

## The fix

Per-doc, two coordinated edits: correct the refusal bullet to name only the
paused-balance refusal, and add a short behavioral note (in "What happens under
the hood") that braid waits/enqueues behind other in-flight ops. This puts each
fact in its natural section and resolves the list-vs-narrative mismatch.

### Edit A -- corrected refusal bullet (all four docs, identical text)

Replace each doc's inaccurate bullet with this canonical line:

```
- Refuses if a btrfs balance is *paused* on the pool -- resume or cancel it first. A paused balance holds the exclusive-operation lock indefinitely, so braid cannot wait it out.
```

Bullets being replaced (the wording differs slightly; `add.md` is the worst --
it enumerates the exact ops that actually enqueue):

- `docs/commands/remove.md:64` -- `- Refuses if a btrfs exclusive operation is already running`
- `docs/commands/add.md:114` -- `- Refuses if a btrfs exclusive operation (balance, device remove, resize) is already running on the pool`
- `docs/commands/replace.md:119` -- `- Refuses if a btrfs exclusive operation is already running`
- `docs/commands/remove-missing.md:81` -- `- Refuses if a btrfs exclusive operation is already running`

### Edit B -- behavioral note in "What happens under the hood" (all four docs)

Append this as a new paragraph immediately after each doc's existing trailing
"A sleep inhibitor is held..." prose:

```
If a btrfs exclusive operation (a running balance, device add/remove/replace, resize, or swap activate) is already in flight on the pool, braid does not fail -- its `btrfs` commands queue behind the in-flight operation (via `--enqueue`) and the kernel runs them when the pool is free. A *paused* balance is the exception and is refused (see Safety checks below).
```

Insertion anchors (the line the new paragraph follows):

- `docs/commands/remove.md:52` -- `A sleep inhibitor is held during data migration and cleanup.`
- `docs/commands/add.md:91` -- `A sleep inhibitor is held during all irreversible operations to prevent the system from suspending mid-operation.`
- `docs/commands/replace.md:101` -- `A sleep inhibitor is held throughout the replace ... can corrupt the btrfs topology.`
- `docs/commands/remove-missing.md:69` -- `A sleep inhibitor is held during the removal and the subsequent soft balance (if triggered).`

(In every doc the "Safety checks / refusal cases" section follows "What happens
under the hood", so "see Safety checks below" resolves correctly.)

### Edit C -- ADR-016 stale helper-name reference

`docs/design/decisions/016-auto-suspend.md:78` references a helper that no longer
exists. Per CLAUDE.md ("Architecture docs describe behavioral contracts, not
internal helper names"), rephrase to the contract rather than just renaming.

Replace:

```
A paused balance holds the btrfs exclusive operation lock. `check_no_exclusive_op` in preflight.rs already treats paused as "refuse." Same logic in `braid idle` — don't suspend mid-pause.
```

With:

```
A paused balance holds the btrfs exclusive-operation lock. The mutating-command preflight in `preflight.rs` already treats a paused balance as a hard refusal (it can block indefinitely, so braid cannot enqueue behind it). Same logic in `braid idle` -- don't suspend mid-pause.
```

(Drops the stale `check_no_exclusive_op` name; also swaps the `—` em-dash for
`--` per the CLI/docs ASCII style rule.) Confirmed this is the only live
reference to the renamed helper -- all other hits are archival `plans/impl/`
files, which are historical records and must not be edited.

## Files to modify

- `docs/commands/remove.md` (Edits A + B)
- `docs/commands/add.md` (Edits A + B)
- `docs/commands/replace.md` (Edits A + B)
- `docs/commands/remove-missing.md` (Edits A + B)
- `docs/design/decisions/016-auto-suspend.md` (Edit C)

Do **not** touch `docs/commands/lock.md` (already accurate).

## Verification

1. **Accuracy review:** re-read each corrected bullet + note against
   `RejectPausedBalanceElseEnqueue` (`preflight.rs:196-201`) and
   `require_mutation_preflight` (`preflight.rs:599-619`). The docs must say:
   paused balance -> refused; any other in-flight op -> wait/enqueue.
2. **Regression grep:** `rg "exclusive operation is already running" docs/`
   returns **zero** matches (all four bad bullets gone). `lock.md` uses
   different, correct wording and will not match.
3. **Stale-ref grep:** `rg "check_no_exclusive_op" docs/` returns **zero**
   matches (only ADR-016 had a live one).
4. **Docs build / link check:** `mdbook build docs` succeeds (mdbook-linkcheck
   validates cross-links per `docs/book.toml`). Edits add no links, so this is
   a safety net, not an expected break.
5. **No Rust run needed:** docs-only change; behavior is already covered by
   `mutation_preflight_rejects_balance_paused` and
   `mutation_preflight_busy_op_returns_info_note` in `cli/src/preflight.rs`.
