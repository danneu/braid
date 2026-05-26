# Plan: align docs + ADR with replace's real `--missing-id` behavior

## Context

`braid replace` documents `--missing-id` as **required** to disambiguate when
multiple devices are missing. The code does not work that way. In
`resolve_replace_source` (`cli/src/replace.rs:1686-1760`), the missing-path
devid is resolved from `--old`'s persisted `pool.json` entry
(`old_member.devid`) and cross-checked against btrfs `missing_devids`
**regardless of how many devices are missing** -- the deliberate comment at
`replace.rs:1754-1759` states the persisted value "already picks the right
one." `--missing-id` is therefore **optional and never required**; when
supplied it is a pure cross-check that must equal the persisted devid (else
`OldDevidMismatch`, the typo guard at `replace.rs:121`). It also cannot rescue
a missing devid: if `old_member.devid` is `None`, replace fails with
`OldMemberMissingDevid` at `replace.rs:1689` *before* the `--missing-id`
branch, whether or not the flag is passed.

The result is a three-way inconsistency: the user guide
(`docs/commands/replace.md`), the **Active** authoritative ADR
(`docs/design/decisions/012-intent-cli.md:66`), and the code all disagree. The
ADR faithfully recorded the *original* intent (a hard multi-missing block, per
the predated `plans/impl/2026-01-01-predated/intent-refactor.md:309`); the code
later evolved to name-based resolution but the docs were never updated.

This is a **pivot** on the original finding: the finding correctly spotted the
docs bug but (a) missed the Active ADR and the flags-table row, and (b) its
proposed replacement text ("`--missing-id` is only needed when `pool.json`
lacks a usable devid") is itself false -- that case errors out regardless of
the flag. Outcome: docs/ADR/comments describe the real behavior, no code or
behavior changes.

## Scope

Docs + one ADR bullet + one test-comment preamble + one clap help string. **No
Rust logic changes. No behavior change.** The code is already correct.

Confirmed clean (no edit): `README.md`, `docs/guides/troubleshooting.md`,
`docs/guides/recovery-scenarios.md` prose, `docs/commands/status.md`,
`cli/src/doctor.rs:770`, `cli/src/add.rs:867` -- these either don't mention the
requirement or use `--missing-id` only as a valid example.

## Edits

### 1. `docs/commands/replace.md` (4 spots)

**Line 20** -- drop the "only one device" framing:
- Before: `Replace a dead/missing disk (auto-detects devid when only one device is missing):`
- After: `Replace a dead/missing disk (the missing devid is resolved automatically from --old's pool.json entry):`

**Lines 36-43** -- reframe the multi-missing example as the optional
cross-check it actually is (keep the command block as-is):
- Before heading: `Replace a dead disk when multiple devices are missing (must specify which):`
- After heading: `Optionally assert which missing devid you expect (braid refuses if it disagrees with pool.json):`

**Line 75** (flags table row) -- correct the parenthetical:
- Before: `Target a specific missing device by btrfs devid (required when multiple devices are missing)`
- After: `Optional cross-check for a dead-disk replace: assert the missing btrfs devid. braid refuses if it disagrees with the devid pool.json records for --old. Never required.`

**Line 112** (safety-checks list) -- replace the false refusal bullet with the
two real ones (sits next to the existing `points to a live device` bullet at
line 111):
- Remove: `- When multiple devices are missing: requires --missing-id to disambiguate`
- Add: `- For missing replacements: refuses if --missing-id disagrees with the devid pool.json records for --old (--old already identifies which member to rebuild)`
- Add: `- For missing replacements: refuses if pool.json has no recorded devid for --old -- --missing-id cannot substitute, it must match the recorded devid`

### 2. `docs/design/decisions/012-intent-cli.md:66` (correct + rationale)

- Before: `- When exactly one device is missing, the devid is auto-resolved. Multiple missing devices require explicit --missing-id.`
- After: `- The missing devid is auto-resolved from --old's persisted pool.json devid, cross-checked against PoolState::missing_devids -- independent of how many devices are missing. Because --old's name already identifies the member, no missing-count gate is needed; --missing-id is an optional cross-check (it must equal the persisted devid, else OldDevidMismatch) and is never required.`

The rationale clause ("Because --old's name already identifies the member...")
records why the original hard-block intent was superseded. ADR status stays
`Active`; only this stale sub-bullet changes.

### 3. `cli/src/main.rs:334` (clap help, enrich)

- Before: `/// Target a specific missing device by btrfs devid (dead disk only)`
- After: `/// Optional cross-check for a dead disk: assert the missing btrfs devid; must match the devid recorded for --old`

(Clap arg field `missing_id: Option<u64>`, `ReplaceArgs`. Use `--`, no
em-dash, per CLI output style.)

### 4. `tests/cli/replace-dead-disk.py` preamble (reword stale framing; logic unchanged)

The test legitimately exercises `--missing-id` (Phase 2: a wrong-devid
rejection subtest at line 180 and a correct-devid subtest at line 194), so only
the misleading preamble wording changes:

- Lines 7-9: drop the "single missing -> ReplaceSource::Missing auto-resolved"
  causal implication. Reword to: `Both auto-detect (devid resolved from --old's
  pool.json entry) and explicit --missing-id <devid> paths use btrfs replace
  start to rebuild from RAID redundancy.` (also removes the existing Unicode
  arrow in favor of ASCII.)
- Lines 20-21: `replaced using --missing-id to disambiguate` ->
  `replaced with an explicit --missing-id to exercise the devid cross-check
  path.`

## Out of scope

- `cli/src/doctor.rs:770` recommends `braid replace ... --missing-id <devid>`.
  This is a valid, working command and the devid is useful context, so leave it.
- No change to `resolve_replace_source` or any planner/executor logic. The
  finding's value is purely doc/ADR accuracy.

## Verification

1. **No stale claim remains:** sweep tracked files only, with phrases specific
   to all five stale locations, so unrelated prose (`require explicit
   --allow-degraded`) and generated `docs/book/` output can't produce false
   positives:
   ```
   git ls-files docs cli tests | xargs rg -n "when only one device is missing|must specify which|required when multiple devices are missing|requires .--missing-id. to disambiguate|Multiple missing devices require explicit"
   ```
   Should return nothing post-edit. (The `.` either side of `--missing-id`
   matches the backticks in `replace.md:112` without putting literal backticks
   inside the double-quoted pattern, which would trigger shell command
   substitution; the trailing-`--missing-id` part of the ADR phrase is dropped
   for the same reason. Run pre-edit it returns the five current hits --
   `replace.md:20,36,75,112` and `012-intent-cli.md:66` -- which is the
   coverage check that the patterns actually match every target.)
2. **Docs still build / cross-links intact:** `mdbook build docs`
   (mdbook-linkcheck runs here per `docs/book.toml`). No links change, so this
   is a regression guard.
3. **CLI still builds and help renders:** `cargo build -p braid-cli` (or
   `just test-rust`), then eyeball `braid replace --help` shows the new
   `--missing-id` help line.
4. **Test comment is comment-only:** the Python edit touches only `#` lines, so
   re-running the 20-30 min `just test-vm replace-dead-disk` is **not** required;
   a `python -c "import ast; ast.parse(open('tests/cli/replace-dead-disk.py').read())"`
   syntax check is sufficient. The unit tests in `cli/src/replace.rs`
   (`missing_id_disagrees_with_persisted_devid`,
   `missing_path_without_persisted_devid_rejected`, etc.) already pin the
   behavior the docs now describe and need no changes.

## Follow Up

- `cli/src/replace.rs:117` says `OldDevidMismatch` can cover a persisted `devid = None`, but the missing-path code returns `OldMemberMissingDevid` before the `--missing-id` branch in that case.
