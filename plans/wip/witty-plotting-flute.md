# kb/ — knowledge base for citation-graph synthesis (Phase 1, minimal)

## Context

`docs/` currently mixes four kinds of content:

1. Authoritative braid-authored design (`principles.md`, `decisions/*.md`, `1-user-stories.md`)
2. Synthesis of external/empirical knowledge (`tool-behavior/`, `real-world/`, plus 5 top-level files like `btrfs-balance-profiles.md`, `claude-enospc-vs-hang.md`)
3. Dev process docs (`tui-insta-guide.md`)
4. Scratch notes (one `.txt` chat transcript — already deleted)

The user wants a **citation graph** so decisions become auditable: source-of-truth → derived insight → decision. Right now, the synthesis-shaped docs in `docs/` cite external URLs and code lines informally with no shared structure and no formal link from ADRs back to the evidence that informed them.

The goal is a `kb/` tree alongside `docs/`, holding LLM-maintained insight pages with inline evidence markers next to claims. ADRs in `docs/decisions/` will eventually be able to cite kb insights as their evidence base. This plan establishes the foundation by migrating exactly one empirical doc and writing a minimal schema.

Insights come in two purposes — `decision-evidence` (cited by an ADR) and `documentation` (records real-world behavior without an ADR attached). Both are valid.

## Current state (already on disk before plan mode)

The following changes already happened during the audit/setup turn before the user interrupted to plan:

- Deleted `docs/notes-calculating-used-free-total-pool-space.txt` (raw chat transcript, no longer accurate; user authorized).
- Deleted `docs/references/` tree (empty `btrfs-docs/dev/` artifact; real btrfs RST docs live in `reference/btrfs-progs/Documentation/`; user authorized).
- Created empty skeleton: `kb/`, `kb/raw/`, `kb/insights/`. The empty `kb/raw/` directory will be removed by Phase 1 (out of scope — see below).

No content has been written into `kb/` yet.

## Minimal Phase 1 scope

**One transformation rule**: move one empirical doc from `docs/` to `kb/insights/`, preserve the prose, add inline evidence markers near important claims, update cross-links.

**In scope**:

- `kb/insights/` directory only
- One real migration: `docs/real-world/sata-hot-unplug.md` → `kb/insights/device-states/sata-hot-unplug.md`
- A minimal `kb/AGENTS.md` describing the inline evidence format and the narrow scope
- Removing the empty `kb/raw/` directory created during setup (it is not used in Phase 1)
- Real migration discipline: delete the original, update `docs/index.md`, fix the inbound cross-link

**Out of scope** (deferred to later plans):

- `kb/raw/` for preserved third-party artifacts. Defer until there's an actual artifact to put there. The rule when it lands: `kb/raw/` is reserved for preserved original artifacts (full mailing-list threads, complete log captures, web clippings); never promote already-derived content to it.
- `last_validated:` field. Defer until there's a workflow that maintains it. Stale dates are worse than no dates.
- `cited_by:` frontmatter field. The natural direction is ADR → insight; backlinks can be added later if grep proves insufficient.
- Document-level `confidence:` frontmatter. Once evidence is claim-adjacent, a document-wide confidence field adds more confusion than value for mixed-confidence docs. Confidence lives only in inline evidence markers.
- "Touched code paths" bottom index. It would duplicate inline `Validated by:` lists. Grep is sufficient.
- `reference/snapshots/<tool>/<version>/` immutable upstream layout. Separate infrastructure problem (touches `just fetch-references`). The kb plan only states one rule for upstream citations: use permanent URLs with commit SHAs.
- Bulk migration of the 7 remaining insight-shaped files in `docs/`. Their own plan after Phase 1 validates the format.
- ADR template change (adding an `## Evidence` section).
- ADR backfill (which insights inform which existing decisions).
- `kb/index.md` content catalog. Skip until there are >5 insights.
- Lint tooling.

## Phase 1 steps

### Step 1: Write `kb/AGENTS.md`

The schema doc that tells future LLMs how to be a disciplined kb maintainer. Keep it short — ~80 lines, focused, no fluff.

Sections:

1. **Purpose** — what kb is, what it is not, who maintains what.
2. **Boundary** — `docs/` (braid-authored) vs `kb/insights/` (LLM-maintained synthesis with citations) vs `.claude/memory/` (per-user conversation context). Explicit ownership.
3. **The two insight purposes** — `decision-evidence` (cited by an ADR) vs `documentation` (records behavior, no ADR required). Same shape, no schema-level distinction beyond the `purpose:` field.
4. **Inline evidence marker** — the single annotation format used throughout insight bodies:
   ```markdown
   > **Evidence**: <what proves the claim is true>
   > **Validated by**: <braid code/tests that depend on the claim>   (optional — omit when nothing depends on it)
   > **Confidence**: hardware-verified | vm-verified | code-read | upstream-doc | community | speculation
   ```
   Place the marker immediately after the claim, section heading, or paragraph it backs. The `Evidence:` and `Validated by:` fields are semantically distinct and must NOT be conflated:
   - `Evidence:` is *what proves the claim is true* (hardware setup, raw artifact, source line, test, doc URL).
   - `Validated by:` is *what braid code or tests depend on the claim* — the reverse-lookup index for "if this claim changes, what should we re-verify?".
   
   For tables that mix confidence levels or have row-specific dependencies, add `Confidence` and (optionally) `Validated by` columns instead of a separate marker. `Evidence` columns are also fine when rows have distinct sources.
5. **Confidence hierarchy** — strongest to weakest, with required-evidence rules:
   - `hardware-verified` — observed on real hardware. Evidence is *either* a `tests/hw/` test path *or* an in-body section describing the hardware setup (machine, drives, OS, what was done) so the observation is reproducible.
   - `vm-verified` — exercised by an automated test under `tests/` or `tests/repro/`. Evidence is the test file path.
   - `code-read` — grounded in source we can read. Evidence is *either* a `cli/src/...:Lline` citation in braid source, *or* an immutable upstream citation (permanent URL with commit SHA — never a floating `reference/` path).
   - `upstream-doc` — from official upstream documentation. Evidence is a permanent URL with commit SHA. Never a floating `reference/.../Documentation/` path.
   - `community` — from a forum, mailing list, or blog. Evidence is the source quoted inline with a permanent URL (or a `kb/raw/` path once that layer exists in a future phase).
   - `speculation` — reasoned but unverified. No evidence required, but the marker must explicitly carry `Confidence: speculation`.
6. **Upstream citation rule** — `reference/` is overwritten in place by `just fetch-references`, so floating `reference/<tool>/...` paths silently rot on toolchain bumps. kb evidence markers must cite upstream sources via permanent web URLs that include the commit SHA — e.g. `https://github.com/kdave/btrfs-progs/blob/<sha>/cmds/replace.c#L142`. Never plain branch URLs; never floating `reference/` paths. (A future plan will introduce `reference/snapshots/<tool>/<version>/` as an alternative; until it lands, permanent URLs are the only legal form.) braid source citations (`cli/src/...`, `tests/...`) are *not* subject to this rule — braid's own git history is canonical.
7. **Frontmatter** — coarse document-wide metadata, four fields only:
   ```yaml
   ---
   intent: One-line description of what this documents and when to read it.
   purpose: decision-evidence | documentation
   type: synthesis | empirical
   status: draft | active | contested | superseded
   ---
   ```
   Reverse citations to code/tests/raw/upstream-docs do **not** appear in frontmatter. They live in inline evidence markers adjacent to the claims they support.
8. **Insight ↔ ADR graduation rule** — when an insight informs a braid decision, the decision graduates to `docs/decisions/` (an ADR). The ADR will eventually cite the insight in an `## Evidence` section; the ADR template change is deferred to a separate plan, but the rule is documented now so the citation chain is understood.
9. **Cross-link convention** — relative markdown links (e.g. `../../insights/btrfs/foo.md`), not Obsidian-style `[[wikilinks]]`, to stay consistent with the rest of the repo and work with plain GitHub rendering.

### Step 2: Real migration of `sata-hot-unplug.md`

A *real* migration: the original `docs/real-world/sata-hot-unplug.md` is deleted, `docs/index.md` is updated to remove its entry, and the inbound cross-link from `docs/tool-behavior/device-disappearance.md` is updated to point at the new location. There must be exactly one authoritative location for the content at any time.

Why this single doc: it's empirical, hardware-verified, single coherent topic, and has a natural confidence split (hardware-verified body + speculative "Unanswered Questions" section). It exercises the inline evidence marker format including the speculation level on a real piece of content.

Why not a `kb/raw/` entry: the dmesg snippets in the source doc are already extracted/editorialized. Promoting them to `kb/raw/` would create fake provenance. They stay inline as quoted blocks. `kb/raw/` is out of scope for Phase 1 entirely.

**Files to write**:

1. `kb/insights/device-states/sata-hot-unplug.md` — the migrated content from `docs/real-world/sata-hot-unplug.md`.
   - Frontmatter: `intent:` (preserved from source), `purpose: documentation`, `type: empirical`, `status: active`. No `confidence:` at document level.
   - Body: preserve the existing prose. Attach inline evidence markers next to substantive claims, sections, or table rows. Examples of the markers expected:
     - State tables get `Validated by` and `Confidence` columns. Cells cite the relevant braid handler code paths (e.g. `cli/src/probe.rs:190-206`, `cli/src/parse/btrfs_filesystem_show.rs:116`). `Confidence` is `hardware-verified` for observed rows.
     - Section-level markers after headings like "State after replug": `Evidence: hardware (hunk)`, `Validated by: <relevant braid paths>`, `Confidence: hardware-verified`.
     - "Unanswered Questions" section gets a marker right after the heading: `Evidence: none — open question`, `Confidence: speculation`. `Validated by:` is omitted.
   - The existing "Validated Code Paths" closing section is **removed** — its information is now distributed inline as `Validated by:` fields. No bottom index.
   - The existing cross-link to `tool-behavior/device-disappearance.md` is updated to `[../../../docs/tool-behavior/device-disappearance.md](../../../docs/tool-behavior/device-disappearance.md)` — the temporary cross-boundary link until that doc migrates in a future phase.

**Files to delete**:

2. `docs/real-world/sata-hot-unplug.md` — the original.
3. `docs/real-world/` — the now-empty directory.
4. `kb/raw/` — the empty directory created during setup; not used in Phase 1.

**Files to edit**:

5. `docs/index.md` — remove the `## real-world/` section (it had only this one file; the whole subsection goes).
6. `docs/tool-behavior/device-disappearance.md` — update its inbound cross-link from `[real-world/sata-hot-unplug.md](../real-world/sata-hot-unplug.md)` to `[kb/insights/device-states/sata-hot-unplug.md](../../kb/insights/device-states/sata-hot-unplug.md)`. This is the only edit to this file.

After Step 2: there is exactly one authoritative location for the SATA hot-unplug content, `docs/index.md` accurately reflects what's in `docs/`, and the cross-link from the synthesis doc resolves correctly across the boundary.

### Step 3: Stop and evaluate

After Step 2, read `kb/AGENTS.md` and the new insight file together. Check:

- Does the schema actually match what the dogfooded file needed?
- Do the inline evidence markers read naturally, or do they break the flow of the prose enough to argue for a different syntax?
- Are the table `Validated by`/`Confidence` columns workable, or do they make tables too wide?
- Does grepping a code path (e.g. `cli/src/probe.rs:190-206`) land directly on the claim that path validates, not just on the document?
- Does the temporary cross-boundary cross-link from `docs/tool-behavior/device-disappearance.md` resolve and read clearly?

Iterate on `kb/AGENTS.md` based on findings. Then this plan is done; bulk migration of the remaining insight-shaped files becomes a separate plan.

## Critical files

To be created:

- `kb/AGENTS.md` — schema doc
- `kb/insights/device-states/sata-hot-unplug.md` — migrated from `docs/real-world/sata-hot-unplug.md`

To be deleted:

- `docs/real-world/sata-hot-unplug.md`
- `docs/real-world/` (empty after the file deletion)
- `kb/raw/` (empty skeleton dir, not used in Phase 1)

To be edited:

- `docs/index.md` — remove the `## real-world/` section
- `docs/tool-behavior/device-disappearance.md` — update one cross-link

Will not be touched:

- Other insight-shaped `docs/` files (future phase)
- `docs/decisions/*.md` (ADR template change is a separate plan)
- Top-level `AGENTS.md` (no kb pointer added yet, defer until kb has more content)
- `just fetch-references` and the `reference/` layout

## Audit findings (insight-shaped files in `docs/` for future migration)

Confirmed by reading each file. NOT in scope for Phase 1, but recorded so the future-migration plan has the list.

| File | Shape | Notes |
|---|---|---|
| `docs/tool-behavior/device-disappearance.md` | synthesis | future migration; mixes confidence levels in its state table |
| `docs/btrfs-balance-profiles.md` | synthesis with "Sources" URL list | future migration; URLs need permanent SHAs |
| `docs/btrfs-balance-soft.md` | synthesis with "Sources" URL list | future migration; URLs need permanent SHAs |
| `docs/claude-enospc-vs-hang.md` | empirical reproduction recipe, cites test files | future migration |
| `docs/btrfs-luks-sector-size.md` | research ending with a "Decision" section | future migration; needs split into insight + new ADR |
| `docs/luks-unlock.md` | research notes with multiple external URLs | future migration; URLs need permanent SHAs |

## Verification

After Step 2, before declaring Phase 1 done:

- **Single source of truth**: `git status` should show `kb/insights/device-states/sata-hot-unplug.md` added, `docs/real-world/sata-hot-unplug.md` deleted, `docs/real-world/` gone, `kb/raw/` gone, `docs/index.md` modified (removing the real-world section), `docs/tool-behavior/device-disappearance.md` modified (one cross-link). There must be no `docs/real-world/sata-hot-unplug.md` left on disk.
- **No information loss**: diff the body of `kb/insights/device-states/sata-hot-unplug.md` against the deleted source (via `git show HEAD:docs/real-world/sata-hot-unplug.md`). The original prose should be preserved; the differences are additions (frontmatter, inline evidence markers, table `Validated by`/`Confidence` columns, the removed bottom Validated Code Paths section, and the updated cross-link).
- **Inline evidence adjacency**: pick three substantive claims at random from the new file. Each should have an adjacent inline evidence marker (after-heading, after-claim, or table-column).
- **Evidence vs Validated-by separation**: spot-check three markers. `Evidence:` should describe what proves the claim; `Validated by:` should list braid code/tests that depend on the claim. They must not be mixed into a single field.
- **Citation precision**: grep `kb/insights/` for `cli/src/probe.rs:190-206`. The match should land on the specific line/row/section that lists it under `Validated by:` — not just somewhere in a document-wide list. Repeat with `cli/src/parse/btrfs_filesystem_show.rs:116`.
- **No `reference/` citations in kb at all**: grep `kb/` for any literal `reference/`. Should match nothing. This is stricter than the schema (which permits `reference/snapshots/...` once that layout exists), but for Phase 1 the snapshot layout doesn't exist yet and the dogfood doesn't cite upstream source, so the absolute prohibition is the right operational check. It's trivial to verify and avoids regex edge cases for tool names starting with `s` (e.g. `systemd`, `smartmontools`).
- **Mixed-confidence expressed locally**: confirm the "Unanswered Questions" section has an inline marker carrying `Confidence: speculation` and that `Validated by:` is omitted.
- **Cross-link resolves both ways**: from `docs/tool-behavior/device-disappearance.md`, the link to the new `kb/` path resolves. From the new insight file, the link back to `device-disappearance.md` resolves.
- **Schema cold-read**: read `kb/AGENTS.md` cold. If a new agent had only this file and no prior conversation, would they know how to write a new insight, attach evidence to a claim, and decide where to put a new piece of information? If any answer is unclear, the schema needs more.
- **Index accurate**: `docs/index.md` no longer mentions `real-world/`. Grep `docs/` for `sata-hot-unplug` to confirm only `device-disappearance.md` references it, with the updated path.

## Resolved decisions

- **Citation model is claim-adjacent, not frontmatter-based.** Reverse citations live in inline evidence markers next to the claims they support. Frontmatter has only `intent`, `purpose`, `type`, `status` — no `sources:`, no `confidence:`, no `last_validated:`, no `cited_by:`.
- **One inline evidence marker shape**: blockquote with `Evidence:` (what proves the claim), optional `Validated by:` (what braid code depends on the claim), and `Confidence:` (one level from the hierarchy). For tables, add `Confidence` and optionally `Validated by`/`Evidence` columns instead of a separate marker.
- **`Evidence` and `Validated by` are semantically distinct** and must never be conflated. A code path that consumes a hardware observation does not validate the truth of the observation.
- **Confidence hierarchy**: six levels — `hardware-verified` → `vm-verified` → `code-read` → `upstream-doc` → `community` → `speculation`. Lives only at the claim level (in inline markers and table columns), never as a document-wide frontmatter field.
- **`hardware-verified` evidence rule**: accepts either a `tests/hw/` test path or an in-body "Hardware" / "Test conditions" section describing machine + setup + method. The dogfood file uses the in-body description.
- **Upstream citation rule (one line)**: kb evidence markers cite upstream source/docs via permanent web URLs with commit SHAs. Never floating `reference/<tool>/...` paths. Never plain branch URLs. The `reference/snapshots/<tool>/<version>/` infrastructure is a separate plan; until it lands, permanent URLs are the only legal form. braid source/test citations (`cli/src/...`, `tests/...`) are exempt because braid's git history is canonical.
- **Cross-link convention**: relative markdown links, not Obsidian wikilinks.
- **No parallel copies during migration**: every migrated file is removed from `docs/`, the index is updated, and inbound cross-links are rewritten in the same step. Exactly one authoritative location at any time.
- **No `kb/raw/` in Phase 1**: defer until there's an actual preserved artifact to put there. The empty skeleton directory created during setup gets removed.
- **No `last_validated:` field in Phase 1**: defer until there's a maintenance workflow. Stale dates are worse than no dates.
- **No `cited_by:` frontmatter**: defer; the ADR → insight direction is the natural one. Backlinks can be added later.
- **No bottom "Touched code paths" index**: it would duplicate inline `Validated by:` lists. Grep is the reverse lookup.
