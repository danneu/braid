# Plan: close the loop in `enospc-vs-hang.md` (how braid avoids the catastrophe)

## Context

`docs/internals/btrfs/enospc-vs-hang.md` is the analysis-of-record for two
`btrfs device remove missing` failure modes -- instant ENOSPC (recoverable)
and partial-relocation -> transaction abort -> forced read-only
(catastrophic). It documents the catastrophe in full but never says that
braid already prevents it. A maintainer reading the page to answer "are we
exposed to the forced-read-only path?" walks away thinking yes. The page's
own "Test files" section even notes the repros use raw btrfs "not braid"
without explaining why -- and the repro preambles
(`tests/repro/btrfs-remove-enospc.py:9-10`) assert "Both must be prevented
by braid's pre-flight space check," a loop this page never closes.

Verified during investigation:

- braid's mutation preflight refuses the dangerous operation **before the
  pending-op journal is written**. The relocation-space guard lives in
  `cli/src/preflight.rs` and is wired in `remove_missing.rs` (fail-closed)
  and `remove.rs` (two policies, see below).
- ADR 012 (`docs/design/decisions/012-intent-cli.md:70-102`,
  "ENOSPC pre-flight check") already **owns the policy/rationale**: the
  2->1 math, the missing-device trust-shape check, and the
  2-device-1-missing kernel RAID1-min rejection.
- The one behavioral fact **not** in ADR 012: the warn-and-proceed vs
  fail-closed asymmetry on `remove`, which lives only in the
  `remove.rs:670-693` docstring.

Outcome: the page should answer "are we exposed?" precisely, while ADR 012
stays the single source of truth for the policy.

## Decision: Option A -- concise failure-mode -> guard map, defer rationale to ADR 012

Rationale (braid-specific): AGENTS.md makes `docs/design/decisions/` the
authority for rationale/invariants and treats duplicate sources of truth as
a hazard. So this page must not restate ADR 012's policy (that copy would
drift). It adds the one thing it is uniquely positioned to own -- the bridge
from each documented failure mode to the specific guard and policy that
covers it -- and references ADR 012 for the "why."

This also corrects two inaccuracies in the originating finding's proposed
text: it lumped `replace` under the relocation-space check (wrong -- `replace`
rebuilds onto the new disk and is guarded by a target-size check), and it
implied `remove` uniformly refuses (the healthy >=2 path is warn-and-proceed).

## Changes

### 1. `docs/internals/btrfs/enospc-vs-hang.md` -- add `## How braid avoids this`

Insert a new `##` section immediately after `## What makes the difference`
(line ~66) and before `## Reproducing the hang/crash in a VM` (line ~68).
Pedagogically: explain the danger -> state the guard -> then the repro
methodology.

Match house style (verified against the internals tree): the file uses
Unicode glyphs (arrow, multiply, em-dash), so render the proposed text's
ASCII placeholders (`->`, `x`, `--`) as those glyphs when writing. Use
backticked `file.rs::symbol` code references and `##` heading depth.
Cross-references to other docs pages are **plain backticked paths, not
markdown links**: this page sits at depth 2 (`docs/internals/btrfs/`), so a
relative `](../../...)` link to `design/` or `commands/` would trip
`just check-docs`'s no-escape heuristic (CI runs it before `mdbook build`),
and the file carries zero markdown links today. Proposed text:

```markdown
## How braid avoids this

braid's mutation preflight refuses these removals -- before the pending-op
journal is written -- whenever it can *prove* the survivors lack the space to
absorb the target's allocations. The degraded failure-mode-2 path is fully
guarded: `remove-missing` and the 2->1 eviction are fail-closed, so an
operator using braid does not reach the catastrophic path above. The healthy
>=2-survivor case is intentionally warn-and-proceed on an *unprovable* check,
because it falls through to btrfs's clean failure mode 1, never the mode-2
abort. Per path:

- **`remove-missing`** -- the degraded failure-mode-2 scenario exactly.
  Computes RAID1 chunk-pair capacity on the survivors and refuses when it is
  below the chunks allocated on the missing device. **Fail-closed:** any
  probe or parse uncertainty also refuses
  (`cli/src/preflight.rs::check_raid1_relocation_space`, wired in
  `cli/src/remove_missing.rs`).
- **`remove` evicting to a single survivor (2->1)** -- RAID1 no longer
  applies, so braid instead checks the lone survivor can hold the
  post-conversion `data + 2 x metadata + 2 x system` (single + DUP profile).
  **Fail-closed** (`cli/src/preflight.rs::check_single_survivor_capacity`).
- **`remove` with >=2 survivors (healthy)** -- same RAID1 relocation check,
  but **warn-and-proceed** on probe/parse uncertainty. A best-effort miss here
  falls through to `btrfs device remove`, which hits the *clean* failure
  mode 1 (instant ENOSPC), not the failure-mode-2 abort, so the filesystem
  stays intact.
- **`replace` is not subject to this failure mode.** `btrfs replace`
  rebuilds onto the new disk instead of relocating onto survivors; its
  preflight refuses a new disk smaller than the one being replaced
  (`cli/src/preflight.rs::check_replace_target_capacity`).

`braid status` and `braid doctor` surface a proactive advisory
(`cli/src/capacity.rs::enospc_risk_advisory`) one disk-loss *before* a pool
enters this danger zone.

The policy and its rationale are owned by ADR 012's "ENOSPC pre-flight
check" section (`docs/design/decisions/012-intent-cli.md`). See also
`docs/commands/remove-missing.md` and the `braid status` ENOSPC advisory
(`docs/commands/status.md`).
```

### 2. `docs/internals/btrfs/enospc-vs-hang.md` -- close the loop in "Test files"

In the final paragraph (line ~134-138), add one sentence explaining *why*
the repros use raw btrfs, linking the new section:

> They invoke raw `btrfs device remove missing` rather than braid precisely
> because braid's preflight (see "How braid avoids this" above) refuses the
> operation under these conditions -- reproducing the unguarded btrfs
> behavior requires bypassing it.

(Plain prose reference, not a markdown anchor link, to keep this file
link-free and consistent with its existing style.)

### 3. (Secondary, separable) `docs/design/decisions/012-intent-cli.md` -- record the asymmetry

ADR 012's "ENOSPC pre-flight check" section (lines 70-102) is the policy
authority but omits the warn-vs-fail-closed asymmetry (currently only in the
`remove.rs` docstring). Add one sentence so the authority is complete and the
internals page defers cleanly:

> The `>=2`-survivor `remove` path treats relocation-probe uncertainty as
> warn-and-proceed -- a miss falls through to btrfs's clean instant-ENOSPC --
> while `remove-missing` and the 2->1 `remove` path are fail-closed on any
> uncertainty, because a miss there can crash the filesystem read-only with
> `pending-op.json` already written.

This edit is independent: change 1's wording is accurate whether or not it
lands (it sources the asymmetry from behavior + code, and cites ADR 012 for
the overall policy).

## Reuse / authority (do not duplicate)

- Policy/rationale: cite, do not restate, ADR 012 §"ENOSPC pre-flight check".
- Guard symbols already exist: `check_raid1_relocation_space`,
  `check_single_survivor_capacity`, `check_replace_target_capacity`
  (`cli/src/preflight.rs`); `enospc_risk_advisory` (`cli/src/capacity.rs`).
- Advisory prose already exists at `docs/commands/status.md:230-243`;
  reference it rather than re-describing the threshold math.

## Out of scope / guardrails

- No code changes -- the guards are correct; this is a docs gap only.
- Do not claim `replace` uses the relocation-space check (it does not).
- Do not restate ADR 012's 2->1 math, trust-shape check, or 2-device-1-missing
  rejection in the internals page; reference it instead.
- No markdown cross-links in this depth-2 page -- use plain backticked paths.
  A relative `](../../...)` link to `design/` or `commands/` fails
  `just check-docs`'s no-escape heuristic (`.github/workflows/docs.yml:34`).
- Leave the frontmatter `intent`/`status` as-is (still accurate).

## Verification

Run the docs gate as CI does (`.github/workflows/docs.yml`). mdbook and the
linkcheckers live only in the `.#docs` Nix shell (`flake.nix:132-145`), so
each gate must be entered through it:

Run all five docs steps, in `docs.yml` order:

- `nix develop .#docs -c just check-docs` -- SUMMARY.md parity, doc-table
  parity, and the no-escape link heuristic. This is the gate that rejects
  `](../../...)`, so it confirms the plain-path cross-references are clean.
- `nix develop .#docs -c just check-docs-frontmatter` -- source frontmatter
  present/valid (unchanged here, but part of the gate).
- `nix develop .#docs -c just check-code-doc-anchors` -- validates
  `principles.md#anchor` citations; this edit adds none, so it is a no-op.
- `nix develop .#docs -c mdbook build docs` -- mdbook-linkcheck2 validates
  any remaining links/anchors and renders the book; a bad reference fails
  the build.
- `nix develop .#docs -c just check-docs-rendered-frontmatter` -- rendered
  HTML must not leak YAML frontmatter.
- Re-read the rendered section against `cli/src/preflight.rs`,
  `cli/src/remove.rs:670-755`, `cli/src/remove_missing.rs:455-592`,
  `cli/src/replace.rs:1364`, and `cli/src/capacity.rs:38` to confirm every
  symbol name and policy claim is exact.
- `rg -n '\]\(' docs/internals/btrfs/enospc-vs-hang.md` -- expect no matches
  (no markdown links introduced into this link-free page).
- `rg -n "replace" docs/internals/btrfs/enospc-vs-hang.md` to confirm the
  only `replace` mention is the "not subject to this" clarification.
- No Rust/VM tests needed (docs-only, no behavior change).

## Implementation notes

- Change 1: reflowed the line wrapping around `>=2-survivor` so no line
  begins with `>`. The plan's proposed wrapping put `>=2-survivor` at the
  start of a line, which mdbook/CommonMark would parse as a blockquote and
  leak into the rendered HTML. Content is unchanged; `>=` is kept (not `≥`),
  per the plan's glyph list.
- Change 2: rather than literally appending the plan's sentence next to the
  existing "They use raw `btrfs device remove missing` (not braid) ..."
  clause (which would say "raw btrfs, not braid" twice), trimmed that clause
  from the first sentence so the appended why-sentence carries the
  raw-btrfs-vs-braid point once. The plan's sentence is otherwise preserved
  verbatim (glyph-converted).
