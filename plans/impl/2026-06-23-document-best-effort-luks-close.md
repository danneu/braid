# Plan: document the best-effort post-commit LUKS close in remove/replace docs

## Context

`docs/commands/remove.md` step 6 ("Closes the LUKS mapper on the removed disk")
presents the post-commit LUKS close as an unconditional, guaranteed step, like
every other step in the "What happens under the hood" list. The code tells a
different story.

In `RemovePlan::execute` (`cli/src/remove.rs:438-470`) the close runs *after* the
irreversible `pool_remove_device` btrfs commit and is deliberately best-effort:

- `close_mapper_best_effort` (`cli/src/mapper_close.rs:72-106`) returns a `bool`
  that `execute` discards. On failure it prints a `[warn] disk <name>: lock
  failed (...)` row and `execute` proceeds to `save_membership` / `clear_journal`
  / `Ok(())`.
- The close is gated by `probe_observed_mapper_uuid`
  (`cli/src/probe_mapper_uuid.rs:51-131`). Every non-`Owned` outcome (inactive,
  foreign LUKS UUID, probe error, null backing) skips the close and emits a
  `Warning: post-commit close skipped for mapper ...` line via `emit_status`.

So a zero exit does **not** guarantee the mapper is closed, and a stranded open
mapper after a "successful" remove is documented behavior, not a doc violation.
The reason it is best-effort: btrfs has already committed the topology change, so
failing the command on a cosmetic close would be wrong.

The same pattern -- and the same misleading doc phrasing -- exists for live
`braid replace`:

- `cli/src/replace.rs:896-915` uses the identical
  `probe_observed_mapper_uuid` + `close_mapper_best_effort` +
  `warn_close_skipped_inactive` sequence after the committed btrfs replace.
- `docs/commands/replace.md:97` step 7 ("For live replacements: closes the old
  disk's LUKS mapper") states it as an unconditional step, same as remove.

`braid lock` already documents this class of behavior in its user guide
(`docs/commands/lock.md:62-65`: a close failure is "downgraded to a warning",
an unverifiable mapper is left "open instead of closing it ... and still exits
cleanly"). That is the house precedent these two docs should match.

`recover` shares the code pattern (`cli/src/recover.rs:3120-3132`) but
`docs/commands/recover.md` never enumerates the old-mapper close as a guaranteed
step, so there is no misleading claim to correct there -- it is out of scope.
README.md and `docs/guides/` do not describe the per-disk close step, so no sync
is needed there.

**Outcome:** an operator reading either command doc understands that a clean exit
does not prove the mapper closed, and that the warning rows are expected
behavior, not failures.

## Changes

### 1. `docs/commands/remove.md` -- annotate step 6

Keep step 6's lead text terse and add a short clarifying paragraph after the
numbered list (next to the existing "A sleep inhibitor is held during data
migration and cleanup." note), mirroring how `lock.md` keeps edge-case close
behavior out of the happy-path step list.

Proposed paragraph (ASCII, matching the file's existing `--` style):

> The step-6 mapper close is best-effort. Because btrfs has already committed the
> device removal, braid never fails the remove on the close: a failed close
> prints a `[warn] disk <name>: lock failed` row, and a close-time ownership
> probe that cannot prove the mapper is the expected disk -- inactive, a probe
> failure or null backing device, or a foreign LUKS UUID -- skips the close with
> a `Warning:` line. A clean exit therefore does not guarantee the mapper is
> closed.

### 2. `docs/commands/replace.md` -- annotate step 7

Add a parallel clarifying paragraph after the numbered list (the "What happens
under the hood" section ends at step 10), cross-referencing remove.md to stay DRY:

> The step-7 mapper close is best-effort, exactly as in
> [`braid remove`](remove.md): the btrfs replace is already committed, so a
> failed close prints a `[warn]` row, and a close-time ownership probe that
> cannot prove the old mapper is the expected disk -- inactive, a probe failure
> or null backing device, or a foreign LUKS UUID -- skips the close with a
> `Warning:` line. Neither fails the replace, and a clean exit does not guarantee
> the old mapper is closed.

## Notes on wording precision

- The finding's phrase "silently skipped (with a `Warning:` line)" is
  self-contradictory; the corrected docs must say the skip is *announced* with a
  `Warning:` line. No skip path is silent (the execute-side
  `MapperOwnership::Unverified => {}` arm is a no-op only because the probe helper
  already emitted the warning).
- Do not over-promise a specific remediation command. `braid lock` will close an
  orphaned `braid-*` mapper, but the foreign/unverifiable case can be left open by
  lock too (`lock.md:65`), so the docs should state the behavior, not prescribe a
  one-liner fix.

## Out of scope

- No code change. The behavior is correct and intentional (best-effort close
  after an irreversible commit); only the docs are inaccurate.
- `recover.md` (no misleading claim to fix), `README.md` and `docs/guides/`
  (do not describe this step).
- No new tests. This is prose only; behavior is already covered by
  `cli/src/mapper_close.rs` tests, the `cli/src/remove.rs:3524` skip-warning
  assertion, and `cli/src/probe_mapper_uuid.rs` ownership-matrix tests.

## Verification

1. Re-read `cli/src/remove.rs:438-470`, `cli/src/replace.rs:896-915`, and
   `cli/src/probe_mapper_uuid.rs:51-131`, and confirm each new sentence matches
   the outcomes the code produces: Owned -> close (warn-on-failure); Inactive ->
   warn-skip; Unverified -> warn-skip, where Unverified covers status/luksUUID
   probe-command error, status parse failure, null backing device, UUID parse
   failure, and foreign (mismatched) LUKS UUID. The doc prose collapses these
   into "probe failure or null backing device, or a foreign LUKS UUID".
2. `just docs-build` -- mdbook build with `mdbook-linkcheck2`; confirms the new
   `[braid remove](remove.md)` cross-link in replace.md resolves and nothing
   else broke.
3. Sanity-check ASCII: the additions use `--`, `...`, and straight quotes only
   (the docs ASCII check `scripts/docs/check-output-ascii.py` targets
   `cli/src/**` and `modules/**` echo lines, not `docs/commands/` prose, but keep
   the additions ASCII for consistency with the surrounding file).
