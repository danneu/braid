# Plan: honest header for the `braid ups status` test (drop the "parser canary" mislabel)

## Context

`tests/cli/braid-status-ups.{nix,py}` labels itself a "parser canary" /
"pure parser canary" in four places, but the body is overwhelmingly an
end-to-end **wiring** test. Of the 242-line `.py`, only lines 22-47 are
parser round-tripping; lines 49-242 pin failure-branch wiring -- exit
codes, stdout/stderr discipline, wrapper PATH/nut resolution
(`unwrap_braid` + `PATH=/nonexistent`), module `config.json` emission,
and the empty / query-failed / invocation-failed / not-enabled branches.
Git history shows this accreted across ~8 commits while the
parser-canary header stayed frozen from the test's birth commit.

The risk (Low severity, but real): a maintainer trimming slow "canary"
VM tests could delete this believing the golden fixtures subsume it,
silently dropping the only end-to-end coverage of the `braid ups status`
outcome matrix and its per-branch process contracts (exit codes,
stdout/stderr routing) -- the parts with no cheaper substitute.
The sibling it names as a companion, `braid-status-rust.nix`, already
self-labels honestly as an integration test ("bridges unit tests... with
integration") -- this file is the lone outlier.

**Outcome:** make both headers honestly describe the test as the
end-to-end `braid ups status` wiring guard that is *also* the live-tool
NUT parser canary, and document the coverage split (what is unique to this
file vs shared with sibling VM tests vs already covered in Rust) so the
deletion mistake can't happen.

### Two corrections to carry into the wording (do not propagate the
### finding's errors)

1. The originating finding cited an AGENTS.md principle, "Parser canaries
   do not catch wiring bugs." **That string does not exist anywhere in
   the repo** (`grep` for "wiring" across all docs returns nothing; the
   only near-match is `tests/module/add-locked-pool.nix` in an unrelated
   `plan_add` context). The new header must NOT cite such a principle.
2. The finding claimed the JSON-sentinel/exit-code/stderr assertions have
   "no Rust-level equivalent." **Overstated.** `cli/src/ups.rs` already
   unit-tests the sentinel JSON *shapes* (`json_not_enabled_*`,
   `json_query_failed_*`, `json_invocation_failed_*`), the `cmd_ups_status`
   human wording + PATH hint (`cmd_ups_status_invocation_failure_surfaces_typed_error`),
   and the query-boundary classification (`query_ups_returns_*`), plus
   insta snapshots `snapshot_json_{invocation_failed,query_failed,not_enabled}.snap`.
   What is genuinely unique to this file is narrower (see the three-way
   split below). The header must state the accurate split, not the
   overstatement.

## Decision (chosen approach)

- **Style:** keep the existing `.nix` "What / Why" prose form (consistent
  with the named companion `braid-status-rust.nix` and the `.nix`-wrapper
  majority). Do **not** migrate to the canonical `Intent / Why it exists /
  Scenario` form -- that is a separate cleanup, out of scope for a label
  fix, and would make this file inconsistent with its own sibling.
- **Depth:** include the precise three-way coverage split -- unique to
  this file / shared with sibling VM tests / covered in Rust (braid's
  idiomatic gap-callout, mirroring `docs/dev/parser-compatibility.md`).
- **Placement:** full rationale + split lives in the `.nix` (the rich
  "purpose" header); the `.py` gets a concise honest summary that
  cross-references the `.nix`. No duplicated long note.

This is a **comment-only** change. No test logic, node config, fixtures,
`flake.nix` registration, or `justfile` recipe changes.

## Files to modify

### 1. `tests/cli/braid-status-ups.nix` (header comment, lines 1-23)

Four edits within the existing structure:

- **Line 1 title:** `parser-canary for `braid ups status`` ->
  end-to-end `braid ups status` wiring test + live-tool NUT parser canary
  (two lines).
- **`What:` paragraph:** stop saying it only "Asserts the parser
  round-trips the expected status flag"; say it exercises the full
  `{human, json}` x `{ok, empty-status, query-failed, invocation-failed,
  not-enabled}` matrix against the live NUT stack, of which parser
  round-tripping on the pinned `nut` package is one cell.
- **`Why:` paragraph:** keep the existing parser-drift rationale (NUT is a
  pinned parser-critical tool per ADR 010; live-tool mirror of the
  `parse_upsc` golden fixtures). Then extend the existing
  "Without it, a refactor that silently broke `cmd_ups_status`..."
  sentence with the precise split:
  - **Unique to this file (no cheaper substitute):** the `braid ups
    status` `{human, json}` x `{ok, empty-status, query-failed,
    invocation-failed, not-enabled}` output matrix and its per-outcome
    process contracts -- exit codes, `--json` stderr silence, stdout/stderr
    routing, human-vs-json wording per branch, the empty-status warning,
    and the not-enabled stdout hint. No other test drives `braid ups
    status` across these branches.
  - **Shared end-to-end coverage (do NOT cite as "only"):** that the
    wrapper resolves `upsc` on an empty PATH is also covered by
    `tests/cli/tool-versions.py` (`assert_wrapper_finds_upsc`, both module
    + top-level wrappers); that `config.ups` is plumbed from the
    module-emitted `/etc/braid/config.json` through live NUT is also
    covered by `tests/module/ups-preflight-on-battery.py`. This file
    exercises those incidentally, not uniquely.
  - **Already covered in Rust (NOT a reason this is redundant):** the JSON
    sentinel *shapes*, human wording, and query-boundary classification
    are pinned by `cli/src/ups.rs` unit tests + insta snapshots; the
    `parse_upsc` contract by the golden fixtures. None of these exercise
    the end-to-end command.
- **Final paragraph (line 23):** drop "this is a pure parser canary, not a
  shutdown test"; keep the real point -- reuses the single-`.dev` fixture
  pattern but strips pool machinery because it does not exercise
  UPS-triggered shutdown, only `braid ups status`.

Keep the "Companion to `braid-status-rust`" + `just test-parsers`
sentence (the test genuinely is in that recipe because NUT is
parser-critical) -- the retitled line 1 and expanded `Why` now make clear
the surface is broader than parser round-tripping.

### 2. `tests/cli/braid-status-ups.py` (header comment, lines 1-7)

- **Line 1:** `# braid-status-ups parser canary.` -> end-to-end
  `braid ups status` wiring test + live-tool NUT parser canary.
- **Body:** keep the existing summary but make it honest: it runs the full
  `braid ups status` outcome matrix, and beyond round-tripping the parser
  it uniquely guards that command's per-branch process contracts (exit
  codes, `--json` stderr silence, stdout/stderr routing). End with a
  pointer: see `braid-status-ups.nix` for the full rationale and the
  coverage split (including the wrapper-PATH / config plumbing that
  `tool-versions.py` and `ups-preflight-on-battery.py` also cover).

## Explicitly NOT changed (and why)

- `docs/dev/parser-compatibility.md:11` ("including `braid-status-ups`,
  the NUT canary") -- correct *in the parser-drift lane*, which is that
  doc's scope.
- `docs/design/decisions/010-toolchain-pinning.md` -- describes the
  `just test-parsers` recipe, not this file's role.
- `flake.nix` checks registration, `justfile:116` recipe, fixtures,
  `cli/src/ups.rs` tests -- all correct as-is.
- No migration to the canonical `Intent/Why/Scenario` preamble form.

## Conventions to honor

- ASCII only in the comment text (`--`, `'`/`"`, `...`) -- matches the
  existing headers and the user's global style rule. (Test comments are
  exempt from `scripts/docs/check-output-ascii.py`, but stay ASCII
  anyway.)
- File citations in prose use `path#symbol`, not line numbers
  (AGENTS.md). The bullet line numbers above are planning aids, not text
  to bake into the comment.

## Verification

Comment-only, so the goal is "nothing structural broke" + "the header now
reads honestly."

1. **Visual review:** re-read both headers; confirm no *standalone* /
   "pure" parser-canary self-label remains -- the paired "live-tool NUT
   parser canary" label is intended and stays. Confirm the three-way
   coverage split is present and accurate, in particular that the
   wrapper-PATH and config-plumbing items are framed as shared with
   `tool-versions.py` / `ups-preflight-on-battery.py`, not as "only," and
   that no fabricated AGENTS.md principle is cited.
2. **Evaluates / still registered (cheap):**
   `nix eval .#checks.aarch64-darwin.braid-status-ups.drvPath` (or
   `nix flake check --no-build` if available) -- confirms the `.nix` still
   parses and the check attr resolves with the edited `readFile` testScript.
3. **End-to-end (recommended, ~minutes):**
   `just test-vm braid-status-ups` -- confirms the `.py` still loads and
   the VM test passes unchanged (a comment edit must not alter behavior).
4. **ASCII sanity (optional):**
   `python3 scripts/docs/check-output-ascii.py` -- should stay green
   (tests are exempt, but confirms no collateral).
