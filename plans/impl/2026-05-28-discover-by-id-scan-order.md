# Plan: document deterministic by-id scan order in `discover.md`

## Context

`cli/src/discover.rs:318` calls `entries.sort_by_key(|entry| entry.file_name())` on the `/dev/disk/by-id/` read-dir result before scanning. The commit that introduced it (`f6cb357 fix(discover): preserve warnings on scan errors`) explicitly says the scan "keeps iterating after the first label collision so later sibling hazards are still reported deterministically."

`docs/commands/discover.md` step 9 (`docs/commands/discover.md:68`) documents the by-id preference order and "lexicographic tie-breaking" for alias selection, but says nothing about scan order. A `verify-issue` review flagged this as Low-severity doc/code drift: a reader auditing the "stable across reboots" claim against the code finds the alias tie-break but may miss why the label-collision incumbent/challenger naming (step 10) is also deterministic.

The finding's proposed fix was to add a clause to step 9. Close reading of the code shows that step 9's alias selection is in fact order-independent via the explicit `(priority, filename)` tuple comparison at `cli/src/discover.rs:505-509` -- the sort is *not* load-bearing for step 9. The sort IS load-bearing for step 10's `first_collision = Some(label_collision(...))` (`cli/src/discover.rs:489-500`), where the existing/candidate identities depend on iteration order.

The right place to document the sort is at the step where the directory is read (step 3), so both step 9 (already correct) and step 10 (determinism falls out for free) read accurately downstream. UUID-collision reporting (step 11) is already order-independent via an explicit lex sort at `cli/src/discover.rs:528-532` and needs no change.

## Edit

Single file: `docs/commands/discover.md`.

### Step 3 (line 62) -- add "in sorted filename order"

Current:

> 3. Reads all entries in `/dev/disk/by-id/`, skipping partition entries (e.g., `ata-TOSHIBA-part1`).

Proposed:

> 3. Reads all entries in `/dev/disk/by-id/` in sorted filename order, skipping partition entries (e.g., `ata-TOSHIBA-part1`). Sorting up front makes label-collision reporting (step 10) independent of `read_dir` order.

Rationale for the second sentence: without it, the reader has to back-derive why the sort is documented at the read step. One short sentence ties the mechanism (step 3) to its visible payoff (step 10) and dissolves the same future audit question this `verify-issue` finding raised.

### Step 9 (line 68) -- no change

Step 9's "lexicographic tie-breaking" wording is accurate as-is: it describes the `(priority, filename)` comparison that picks the preferred alias, which is order-independent. Editing it would either duplicate the step-3 note or wrongly imply the sort is what makes alias selection deterministic.

### Step 10 (line 69) -- no change

The new sentence in step 3 references step 10 explicitly, so step 10's prose does not need to repeat the cross-reference.

## What this plan does not change

- `cli/src/discover.rs`: no code edit. The sort is intentional and load-bearing -- this is doc-only drift.
- Other docs: `docs/internals/luks-unlock.md:16-18` covers by-id stability via hardware serial numbers (a different mechanism). `status.md` and `add.md` do not enumerate `/dev/disk/by-id/`. `recover.md:79` (step 12) does resolve by-id paths and `cli/src/recover.rs:104-164` (`resolve_by_id_for_underlying`) enumerates `list_by_id_entries()` to find symlinks that canonicalize to a given kernel device. Recover's selection is order-independent via its own explicit `matches.sort_by(...)` on `(priority, filename)` at `cli/src/recover.rs:161`, and it emits no "first encountered" diagnostic, so the discover scan-order note does not apply and no parallel update is needed there.
- `docs/design/principles.md` / `docs/design/decisions/`: principle 5's "deterministic" claim is about mapper names, not by-id scan order; no ADR-level change is warranted for a Low-severity doc clarification.

## Verification

1. `mdbook build docs` -- confirms the edit renders and that no cross-links broke (`mdbook-linkcheck` runs as part of the build per `docs/book.toml`).
2. Visual read of the rendered step 3 -> step 10 flow to confirm the cross-reference reads naturally.
3. No tests, no fixture refresh, no parser canary -- this is a doc-only change inside `docs/commands/`.
