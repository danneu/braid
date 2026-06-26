# Align `replace --missing-id` clap help with the doc's "never required" vocabulary

## Context

A project-fit review flagged that the `--missing-id` clap help string for
`braid replace` describes the flag with an imperative -- "must match the devid
recorded for --old" -- while every other surface frames it as an *optional
cross-check that is never required*. The behavior is correct and undisputed:
`resolve_replace_source` (`cli/src/replace.rs`, see `ReplaceError::OldDevidMismatch`)
auto-resolves the missing devid from `--old`'s persisted `pool.json` entry when
the flag is omitted, and refuses with `OldDevidMismatch` only when a *supplied*
`--missing-id` disagrees. So the flag is a refusal-on-disagreement cross-check,
never a usage requirement.

A codebase-wide sweep confirms this clap string is the **lone outlier**. These
surfaces already use the settled "optional / never required" vocabulary and need
no change:

- `docs/commands/replace.md` flag table + prose ("Optional cross-check ... Never required.")
- ADR `docs/design/decisions/012-intent-cli.md` ("an optional cross-check ... and is never required")
- `cli/src/repair_hint.rs` (`optional_missing_id_cross_check_phrase`, doc-commented "should not imply `--missing-id` is required")
- `cli/src/doctor.rs` recommendation wording and its tests
- `tests/cli/replace-dead-disk.py` preamble

(Note: `braid remove-missing` has a *separate* `--missing-id` that genuinely is
required -- out of scope, do not touch.)

Intended outcome: the `--help` text and the `docs/commands/replace.md` flag row
carry the same constraint in the same words, so a future reviewer reading either
surface reaches the same conclusion and this class of finding stops recurring.

## The change

Single-line edit to the `///` doc comment on `ReplaceArgs::missing_id` in
`cli/src/main.rs` (clap renders this `///` as the flag's `--help` text). Preserve
the good first clause; replace only the flagged trailing imperative so the
sentence adopts the doc row's "refuses / Never required" framing.

**Before:**

```
/// Optional cross-check for a dead disk: assert the missing btrfs devid; must match the devid recorded for --old
```

**After:**

```
/// Optional cross-check for a dead disk: assert the missing btrfs devid. braid refuses if it disagrees with the devid pool.json records for --old. Never required.
```

Concretely: replace `; must match the devid recorded for --old` with
`. braid refuses if it disagrees with the devid pool.json records for --old. Never required.`

### Why this exact wording

- **Maximizes vocabulary alignment** -- the refusal clause and "Never required."
  are lifted verbatim from the `docs/commands/replace.md` flag row, so the two
  surfaces now state the constraint identically. That is the whole point of a
  project-fit fix.
- **Surgical** -- keeps the accurate first half ("Optional cross-check for a dead
  disk: assert the missing btrfs devid") that already leads with "Optional";
  only the awkward `must match` imperative is rewritten.
- **Pure ASCII** (no em-dash / curly quotes), satisfying the CLI-output ASCII
  convention regardless of whether `scripts/docs/check-output-ascii.py` scans
  doc comments.

Rejected alternatives:

- *The review's literal proposal* dropped "assert the missing btrfs devid" (what
  the flag does); keeping it matches the doc row and tells the operator the
  flag's purpose.
- *Re-polishing the wording in both surfaces* (e.g. "--old's pool.json devid"):
  rejected -- it would re-open wording settled one month ago in commit
  `be079e1a` and risks introducing a *new* divergence for no behavioral gain.
  Aligning the outlier to the canonical text is the lower-risk move.

## No other files change

The sweep found no second drifted surface (the one "stale" hit reported against
`docs/commands/replace.md:75` was a false positive -- that row was already
corrected in `be079e1a`; verified by direct read + `git show`). A repo grep for
the clap phrasing returns only `cli/src/main.rs` and the already-correct
`docs/commands/replace.md` row -- no test, golden snapshot, or completion
fixture pins the help string, so blast radius is the single line.

## Verification

1. **Build / lint:** `just test-rust` (or `cargo build` + `cargo clippy` in
   `cli/`) -- no functional code changed; must stay clean.
2. **Help renders as intended:** run `braid replace --help` (or
   `cargo run -- replace --help`) and confirm the `--missing-id` row shows the new
   sentence ending in "Never required."
3. **No pinned-string regressions:** `grep -rn "must match the devid recorded" cli/ tests/ docs/commands docs/design`
   returns nothing after the edit; the only `--missing-id` help text now matches
   the `docs/commands/replace.md` flag row word-for-word on the constraint.
4. **Sanity on the sibling command:** confirm `braid remove-missing --help` is
   untouched (its `--missing-id` is a different, genuinely-required flag).

No new tests: this is a help-string wording change with no behavioral surface to
assert against, and the existing `tests/cli/replace-*.py` already exercise the
optional/auto-resolve and disagreement-refusal paths.

## Implementation notes

- Clap strips the final period from rendered doc-comment help, so the source comment matches the canonical docs row while `cargo run -- replace --help` displays the same words ending in `Never required`.
