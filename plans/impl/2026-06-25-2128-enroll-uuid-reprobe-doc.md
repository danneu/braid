# Plan: document enroll's execute-time UUID re-probe in the "What happens under the hood" list

## Context

A review finding (Low / project-fit) flagged that `docs/commands/enroll.md`'s
numbered "What happens under the hood" walkthrough omits the execute-time LUKS
UUID re-probe -- the decision-024 mutation-boundary guard that
`EnrollPlan::execute` runs over every present pool member after the passphrase
is read but before any slot is mutated. Today it surfaces only as a Safety-checks
bullet (`enroll.md:81`), so a reader walking the ordered list to reason about
*when* a mid-prompt disk swap is caught never sees the second, load-bearing
check in the place they expect it.

The finding's core claim is correct, but its proposed fix is not: it said to
insert the step "between current steps 5 and 6." Step 5 is the passphrase
*verify*; the code runs the re-probe *before* the verify, deliberately, so a
mid-prompt swap reports a clear UUID mismatch instead of a misleading
"wrong passphrase" (`cli/src/enroll_key_file.rs:546-552`). Placing it after the
verify would re-introduce an ordering inaccuracy. This plan is the corrected
pivot: insert the re-probe as the new **step 5**, before the verify step, with
wording precise enough to convey that deliberate ordering.

Outcome: the ordered narrative matches the tested behavior, and the two existing
surfaces (the Safety-checks bullet and ADR 024) gain a consistent third home
without drifting.

## Behavior being documented (already implemented, already tested)

Execute-time order in `EnrollPlan::execute` (`cli/src/enroll_key_file.rs:534`):

1. read passphrase (`:544`)
2. **re-probe each candidate's live LUKS UUID vs `pool.json`** (`:553-555`,
   `reprobe_member_luks_uuid`) <- missing from the doc list
3. verify passphrase (`plan_enrollment` -> `verify_credential_for_targets`, `:332`)
4. keyfile probe + slot-1 check (`:355-361`)
5. (`--generate`) create keyfile (`:570-583`)
6. apply / `luksAddKey` (`apply_enrollment`, `:588`)

The numbered list's planning->execute seam is the 4->5 boundary: list steps 1-4
are `plan_enroll` (pending-journal check, keyfile-target validation, discovery
membership scan + discovery-time UUID check), all pre-passphrase; list step 5
onward is `execute`. So the re-probe inserts cleanly as the new step 5 -- right
after the discovery-time UUID check (step 4) and before the passphrase verify
(current step 5). The two checks bracket the passphrase-prompt window.

No code or test changes. The behavior is already covered, so the doc edit is
purely descriptive and structure-insensitive:
- VM test `tests/cli/enroll-uuid-mismatch-midprompt.py` -- mid-prompt swap (same
  passphrase) is rejected at the execute re-probe, `braid.key` never created,
  slot 1 untouched.
- Rust unit tests `reprobe_member_luks_uuid_mismatch_rejects` (`:3653`) and
  `reprobe_member_luks_uuid_probe_failure_fails_closed` (`:3701`), plus the
  discovery->execute window-closure coverage anchored in ADR 024.

## The change (single file: `docs/commands/enroll.md`)

Under `## What happens under the hood`, insert a new **step 5** between the
current step 4 (discovery-time UUID check) and the current step 5 (passphrase
verify), then renumber the existing steps 5-10 to 6-11.

New step 5 (Option A -- explicit before-verify rationale, ASCII only):

> 5. Reads the pool passphrase, then -- before verifying it and before any slot
>    change -- re-probes each present member's live LUKS UUID against
>    `pool.json`, repeating the discovery-time check (step 4) at the mutation
>    boundary. The passphrase prompt is an operator-controlled window in which a
>    disk could be swapped or reformatted; a mismatch, or a probe that cannot
>    confirm the UUID, aborts before slot 1 is touched. Re-probing before the
>    verify means a mid-prompt swap surfaces as a clear LUKS UUID mismatch
>    rather than a misleading wrong-passphrase error.

Then the current list shifts:
- current 5 ("Verifies the passphrase ... before any keyfile probe.") -> 6
- current 6 (keyfile probe) -> 7
- current 7 (slot-1 check) -> 8
- current 8 (`--generate` keyfile creation) -> 9
- current 9 (enroll keyfile) -> 10
- current 10 (header backup) -> 11

Notes on wording choices:
- Remediation hint ("detach the foreign disk ... or run `braid replace`") is
  deliberately *not* repeated here -- step 4 and the line-81 bullet already
  carry it, and the live mismatch error from `luks::format_luks_uuid_mismatch`
  includes it. Step 5 stays focused on the new fact: the re-check's timing and
  why it precedes the verify.
- No hyperlink to ADR 024: step 4 documents the sibling discovery check inline
  with no link, so the new step matches that voice and avoids a linkcheck
  dependency.

## What intentionally stays unchanged

- **Safety-checks bullet (`enroll.md:81`)** -- already accurate ("after the
  passphrase is read and before any keyfile is enrolled"); it is the quick-scan
  guarantee register, complementary to the narrative step, and does not
  contradict "before the verify." Leave as-is.
- **ADR 024 (`docs/design/decisions/024-luks-uuid-identity.md:343-349`)** -- the
  live authority for this guard; already describes the enroll re-probe. No edit.
- **README.md** -- enroll appears only as a bare commands-table row (no flow
  narrative); nothing to sync. (Confirmed: no parallel ordered-step description
  exists anywhere else in `docs/`.)

## Verification

- `just docs-build` -- builds the mdBook and runs `mdbook-linkcheck2`; confirms
  the page renders and no links broke (none added, but the gate must stay green).
- `python3 scripts/docs/check-line-cites.py` and the other `scripts/docs/*.py`
  checks if not already wired into `docs-build` -- the edit adds no line-number
  citations and no new tables, so these should pass untouched.
- Eyeball the rendered list: 11 sequential steps, step 5 (re-probe) flows into
  step 6 (verify), no duplicate or skipped numbers.
- No Rust/VM test run required for correctness (no behavior change); optionally
  confirm the described behavior still holds by reading
  `tests/cli/enroll-uuid-mismatch-midprompt.py`, which already encodes it.
