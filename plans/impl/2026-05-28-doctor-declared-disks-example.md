# Plan: fix stale `disk(s)` example in `docs/commands/doctor.md`

## Context

Commit `ff8235a refactor(cli): remove literal plural markers from output`
(2026-05-14) switched `summarize_declared_disks` in `cli/src/doctor.rs`
from `"all {total} declared disk(s) present"` to grammatical pluralization
(`disk` for `total == 1`, `disks` otherwise), and updated the pinning unit
test (`summarize_ok_when_all_headers_intact`, asserts `"all 2 declared
disks present"`). The doc example in `docs/commands/doctor.md:25` was
missed by that commit and still shows the obsolete `disk(s)` form for a
3-disk pool. The verify-issue investigation confirms this is the only
stale literal-CLI-output instance in `docs/` (the `disk(s)` occurrence in
`docs/guides/recovery-scenarios.md:232` is English prose, not quoted
output, and stays).

Goal: bring the documented example into agreement with the emitter and
its unit test so a reader who greps for `disk(s)` against real output
doesn't get a false mismatch.

## The fix

Edit `docs/commands/doctor.md:25` from:

```
[ok]   declared disks  all 3 declared disk(s) present
```

to:

```
[ok]   declared disks  all 3 declared disks present
```

This matches what `cli/src/doctor.rs:431-438` emits for any `total >= 2`
pool, and matches the form pinned by the unit test at
`cli/src/doctor.rs:3121`.

No code or test change is required. `summarize_declared_disks` already
handles the singular case, and there is no `total == 1` example in the
doc to keep in sync.

## Out of scope

- Sibling failure-path message (`"{}/{} disks have problems: ..."`) was
  also degrammaticalized in the same upstream commit, but the doctor
  doc does not show that string, so there is nothing to update there.
- `docs/guides/recovery-scenarios.md:232` (`"the surviving disk(s)"`)
  is English shorthand for "one or more disks", not quoted CLI output.
  Leave it.

## Verification

- `git grep -n 'disk(s)' -- docs/ ':(exclude)docs/book'` should return
  only the `recovery-scenarios.md` prose line after the edit.
- `mdbook build docs` succeeds (no broken cross-links; the example is
  inside a fenced code block, not a link).
- No Rust tests are affected. `just test-rust` is unnecessary but
  harmless to run as a smoke check.
