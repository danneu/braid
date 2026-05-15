# Plan: drop "backward compat" wording from multi-add test

## Context

The `multi-add` VM test labels its Phase 3 subtest as "single-disk add ->
backward compat". The framing implies single-disk add is a legacy path
that multi-disk add replaced, which contradicts braid's actual posture:
both single- and multi-disk add are first-class, and braid is unreleased
software that does not carry backward compatibility (`AGENTS.md:45-47`,
"No backwards compatibility").

The Phase 3 subtest itself is correct -- it verifies that
`braid add disk5` works against a pool that was created and expanded via
the multi-disk path. Only the comment/header wording is wrong: it seeds
the wrong mental model and pollutes grep results when someone searches
for "backward compat" looking for real compat shims to remove.

The originating finding only cites `tests/cli/multi-add.py:79`, but the
same misframed wording is duplicated verbatim in
`tests/cli/multi-add.nix:6`. Fixing only the `.py` file would leave the
sibling inconsistent, so this plan covers both.

## Changes

Comment-only edits. No code, fixtures, or runtime behavior change.

### `tests/cli/multi-add.py`

- **Line 6** (preamble bullet 3): replace
  `(3) single-disk add to existing pool → backward compat.`
  with
  `(3) single-disk add to an existing pool.`

- **Line 79** (Phase 3 header): replace
  `# --- Phase 3: Single-disk add → backward compat ---`
  with
  `# --- Phase 3: Single-disk add to an existing pool ---`

### `tests/cli/multi-add.nix`

- **Line 6** (preamble bullet 3): replace
  `(3) single-disk add → backward compat.`
  with
  `(3) single-disk add to an existing pool.`

Both files use the literal arrow `→` already; that character is
preserved (the surrounding file already uses it, per the global rule in
`~/.claude/CLAUDE.md`). The scope of this cleanup is the
`backward compat`/`backwards compat` wording only -- `legacy` is **not**
being scrubbed. `tests/cli/` contains many legitimate `legacy`
references (warning-prefix regression tests such as
`braid-add-warnings.py`, `braid-remove-softwarn.py`,
`braid-add-enroll.py`, `replace-preview-warnings.py`,
`braid-remove-missing-softwarn.py`, plus the `pool.json` migration test
`braid-discover-migration.py`), all of which are correctly describing
real legacy behavior and are out of scope here.

## Critical files

- `tests/cli/multi-add.py` -- preamble + Phase 3 header.
- `tests/cli/multi-add.nix` -- preamble.

## Verification

1. `rg -n 'backward compat|backwards compat' tests/cli/` returns no
   matches. (No `legacy` grep cleanup is claimed -- existing `legacy`
   references in `tests/cli/` are out of scope and stay as-is.)
2. Visual diff review is the meaningful verification: only comment
   lines changed; no executable lines or `subtest()` strings touched.
   The existing `subtest("Single disk add to existing pool works")` on
   line 81 already matches the new framing, so the file becomes
   internally consistent.
3. Optional: `just test-vm multi-add` as a parse/sanity check. Not
   required, since the change is comment-only and behavior is
   unchanged.
