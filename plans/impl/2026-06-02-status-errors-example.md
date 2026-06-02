# Fix per-disk `Errors:` example in status docs

## Context

A High/Accuracy review finding flagged that the per-disk `Errors:` line in
the `braid status` reference doc shows a format the tool never produces.

- **Doc** (`docs/commands/status.md:152`):
  `Errors:  read=0 write=0 flush=0 corruption=0 generation=0`
  (`key=value`, space-separated)
- **Code** (`cli/src/status.rs:1432-1435`):
  `"    Errors:  read {} / write {} / flush {} / corruption {} / generation {}\n"`
  renders as `Errors:  read 0 / write 0 / flush 0 / corruption 0 / generation 0`
  (`key value`, ` / `-separated)

The drift is original, not a regression: the doc form was born in the
doc-creation commit (`36541831`) and the code form in `9a14b6f6 impl phase 5`;
they have never matched. The code is the authoritative shipped behavior -- two
tolerance tests (`cli/src/status.rs:3770` and `:3798`) assert
`human.contains("Errors:  read 0")`. The intended outcome: the doc example
matches real `braid status` output.

## Direction: fix the doc, not the code

The code output is correct, tested, and perfectly readable; the only defect is
that the example drifted from it. Reconciling toward the doc would mean a
user-facing output change plus test churn for zero functional or UX gain. So
the ideal fix is the minimal, zero-risk one: correct the doc.

## The change

Single-line edit in `docs/commands/status.md` (inside the "Per-disk detail"
code fence, line 152). Preserve the existing 4-space indent; only the text
after `Errors:  ` changes.

- Before: `    Errors:  read=0 write=0 flush=0 corruption=0 generation=0`
- After:  `    Errors:  read 0 / write 0 / flush 0 / corruption 0 / generation 0`

This is the verbatim output of the `status.rs:1433` format string with
all-zero counts, matching the `present` example disk in the same fence.

### Scope is exactly one line -- confirmed isolated

- The rest of the per-disk block (`Device:`, `Model:`, `Serial:`, `LUKS:`, the
  `{:<18}`/`{:<10}` name+devid columns, the MISSING `(not found)` branch) was
  compared against the code and already matches exactly.
- Repo-wide `rg`: `Errors:` appears once in `docs/` and not in `README.md`; the
  `read=` form exists nowhere else; the ` / ` form exists only at
  `status.rs:1433`. No sibling examples to update.

## Rejected alternatives

- **Change the code to `key=value`.** A behavior change to shipped, tested
  output, chosen between two equally-readable forms on aesthetics alone. Churn
  (tests at `:3770`/`:3798` would need updating), not improvement.
- **Add an anti-drift guard** (Rust test asserting the doc file contains the
  rendered line, or generating the example from code). Over-engineering for a
  single example line; couples a test to doc prose and breaks on harmless
  rewording.

## Verification

- Confirm the edited line byte-matches the `status.rs:1433` format string with
  zeros substituted (it does, by construction).
- `mdbook build docs` still succeeds (the docs cross-link check is unaffected by
  fenced-example content).
- No code change, so the existing tests at `cli/src/status.rs:3770` and `:3798`
  continue to pin the real output; no new or changed test is needed.
