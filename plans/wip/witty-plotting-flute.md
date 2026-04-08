# kb/ — knowledge base for citation-graph synthesis

## Context

`docs/` currently mixes four kinds of content:

1. Authoritative braid-authored design (`principles.md`, `decisions/*.md`, `1-user-stories.md`)
2. Synthesis of external/empirical knowledge (`tool-behavior/`, `real-world/`, plus 5 top-level files like `btrfs-balance-profiles.md`, `claude-enospc-vs-hang.md`)
3. Dev process docs (`tui-insta-guide.md`)
4. Scratch notes (one `.txt` chat transcript — already deleted)

The user wants a **citation graph** so decisions become auditable: `raw source (immutable) ← derived insight (synthesizes) ← decision (cites)`. Right now, the synthesis-shaped docs in `docs/` are doing half of this implicitly — they cite external URLs and code lines, but there's no curated raw layer, no schema for confidence/staleness/applies-to, and no formal link from ADRs back to the evidence that informed them.

The goal is a `kb/` tree alongside `docs/`. `kb/raw/` holds immutable third-party sources (web clippings, mailing-list threads, dmesg captures). `kb/insights/` holds LLM-maintained synthesis pages with citations to `kb/raw/`, `reference/`, braid source, and tests. ADRs in `docs/decisions/` will eventually gain an `## Evidence` section that links to `kb/insights/` pages.

Insights come in two purposes — `decision-evidence` (cited by an ADR) and `documentation` (records real-world behavior without an ADR attached). Both are valid.

## Current state (already on disk before plan mode)

The following changes already happened during the audit/setup turn before the user interrupted to plan:

- Deleted `docs/notes-calculating-used-free-total-pool-space.txt` (raw chat transcript, no longer accurate; user authorized).
- Deleted `docs/references/` tree (empty `btrfs-docs/dev/` artifact; real btrfs RST docs live in `reference/btrfs-progs/Documentation/`; user authorized).
- Created empty skeleton: `kb/`, `kb/raw/`, `kb/insights/`.

No content has been written into `kb/` yet.

## Audit findings (insight-shaped files in `docs/` to migrate eventually)

Confirmed by reading each file. Categorized for later migration but NOT in scope for Phase 1.

| File | Shape | Notes |
|---|---|---|
| `docs/tool-behavior/device-disappearance.md` | synthesis, has `intent:` frontmatter, has "Code Pointers" reverse citations, has explicit "??" uncertainty markers, mixes confidence levels in its state table | future migration — Phase 2 (when migrated, exercises claim-level confidence in tables) |
| `docs/real-world/sata-hot-unplug.md` | empirical, has `intent:` frontmatter, has "Validated Code Paths" reverse citations, has "Unanswered Questions" section, inlines dmesg snippets | **Phase 1 dogfood** — single empirical doc, mixes hardware-verified body with speculative Unanswered Questions |
| `docs/btrfs-balance-profiles.md` | synthesis with "Sources" URL list | future migration |
| `docs/btrfs-balance-soft.md` | synthesis with "Sources" URL list | future migration |
| `docs/claude-enospc-vs-hang.md` | empirical reproduction recipe, cites test files | future migration |
| `docs/btrfs-luks-sector-size.md` | research ending with a "Decision" section | future migration; **needs split** into insight + new ADR |
| `docs/luks-unlock.md` | research notes with multiple external URLs | future migration; URLs should become `kb/raw/` clippings |

These will all stay in place during Phase 1 to avoid bundling. They get migrated in Phase 2 once the schema is validated.

## Scope of this plan

**Phase 1 only**: write the schema doc, do one *real* migration of a single empirical doc, then stop and evaluate. "Real" means the original is removed and the index updated — no parallel copies, no dual sources of truth. If the schema feels wrong after dogfooding, iterate on it before any bulk migration.

Two constraints learned from plan review:

1. **No parallel copies during migration.** Every migrated file gets removed from `docs/`, `docs/index.md` is updated, and any inbound cross-links from other `docs/` files are updated to point at the new `kb/` location. There is exactly one authoritative location for any given content at any time.
2. **No fabricated raw provenance.** `kb/raw/` is reserved for *preserved original artifacts* (full mailing-list threads, complete log captures, web clippings). Do not promote already-derived content (selected dmesg lines that were extracted into a synthesis doc) to `kb/raw/`. If the original capture no longer exists, the content stays inline in the insight; the raw layer waits until a real artifact comes along.

Out of scope (deferred to later plans):

- Bulk migration of the 7 remaining insight-shaped files
- ADR template change (adding `## Evidence` section)
- ADR backfill (which insights inform which existing decisions)
- `kb/index.md` (skip until there are >5 insights)
- Lint tooling
- `qmd` or other search

## Phase 1 steps

### Step 1: Write `kb/AGENTS.md`

The schema doc that tells future LLMs how to be a disciplined kb maintainer. Match the tempo of the existing top-level `AGENTS.md` (focused, ~150 lines, no fluff).

Sections (with one-line intent each):

1. **Purpose** — what kb is, what it is not, who maintains what.
2. **Boundary table** — `docs/` vs `kb/raw/` vs `kb/insights/` vs `.claude/memory/` vs `reference/`. Explicit ownership and what goes where.
3. **The two insight purposes** — `decision-evidence` (must be cited by an ADR) vs `documentation` (records real-world behavior, no ADR required). Same shape, different lint rules.
4. **Operations** — `ingest`, `query`, `lint`. One paragraph each.
5. **Insight page format — frontmatter** is coarse document-wide metadata only: `intent`, `purpose`, `type`, `status`, dominant `confidence` (informational only — does not substitute for claim-level evidence), optional `last_validated`, optional `applies_to`, optional `cited_by`. **`sources:` is intentionally NOT in frontmatter.** Reverse citations to braid code, tests, raw artifacts, and upstream docs live next to the claims they support, not in document-wide lists.
6. **Local evidence blocks (the primary citation model)** — every substantive claim, table row, or section that has specific backing carries a local evidence block adjacent to it. The block has two distinct citation fields that must NOT be conflated:

   - **`Evidence:`** — *what proves the claim is true*. The source-of-truth for the claim itself: a hardware setup description, a `kb/raw/...` artifact path, a `reference/...` upstream source line, an upstream doc URL, a `tests/...` test path that asserts the behavior, or a `cli/src/...:Lline` line *only when the claim is about that source itself* (e.g., a code-read claim about how the parser works).
   - **`Validated by:`** — *what braid code or tests depend on the claim*. The reverse-lookup index: paths under `cli/src/...:Lline` and `tests/...` that would need to be re-verified or updated if the claim were false or changed. Optional — omit when no braid code currently depends on the claim.
   - **`Confidence:`** — one level from the hierarchy (section 8). Required.
   - **`Last validated:`** — optional, claim-level. Date of the underlying observation/test/code-read. Omit when unknown — never substitute a doc-edit date.

   The semantic distinction matters: a code path does not validate the *truth* of a hardware observation; it only identifies braid logic that depends on the observation being true. Conflating them would weaken staleness review and citation precision.

   Three placements depending on shape:

   **a) Section-level** — block sits right after the heading, before the section's prose:
   ```markdown
   ### State after replug
   > **Evidence**: hardware test on `hunk` (3x SATA HDD btrfs RAID1 over LUKS, NixOS) — single observation, date unknown
   > **Validated by**: `cli/src/probe.rs:190-206`, `cli/src/monitor.rs:48-70`
   > **Confidence**: hardware-verified

   The LUKS mapper does not recover from null-underlying after replug...
   ```

   **b) Table columns** — for tables whose rows have distinct evidence, add explicit `Evidence` and `Validated by` columns; add `Confidence` when rows mix levels:
   ```markdown
   | State | btrfs filesystem show | braid maps to | Evidence | Validated by | Confidence |
   |---|---|---|---|---|---|
   | Null-underlying | path /dev/mapper/X | pool.null_underlying | hardware (hunk) | `cli/src/probe.rs:190-206` | hardware-verified |
   | MISSING with path | path /dev/mapper/X MISSING | gap | none observed; reasoned from code | `cli/src/parse/btrfs_filesystem_show.rs:116` | community |
   ```

   **c) Per-claim** — block sits immediately after a single non-section claim that has its own backing:
   ```markdown
   btrfs still lists the device after hot-unplug — it doesn't know the device is gone until I/O fails.
   > **Evidence**: hardware (hunk)
   > **Validated by**: `cli/src/probe.rs:190-206`
   > **Confidence**: hardware-verified
   ```

   **Where local blocks are NOT required**: claims whose evidence is identical to the dominant document-level confidence with no specific backing to attribute and no braid dependency don't need a block. Use blocks where they sharpen attribution.

7. **Raw page format** — frontmatter spec (`source_url`, `source_type`, `fetched_date`, `participants`, `tool_versions`) + body rule (verbatim, no editing, mark elisions). Hard rule: never promote already-derived content to `kb/raw/`. If you don't have a preserved original artifact, keep the content inline in the insight.
8. **Confidence hierarchy** — table from strongest to weakest (`hardware-verified` → `vm-verified` → `code-read` → `upstream-doc` → `community` → `speculation`) with required-evidence rules:
   - `hardware-verified` — observed on real hardware. Evidence is *either* a `tests/hw/` test path, *or* an in-body section describing the hardware setup (machine, drives, OS, what was done) so the observation is reproducible. A `kb/raw/` capture (preserved log/output) is also acceptable when one exists.
   - `vm-verified` — exercised by an automated test under `tests/` or `tests/repro/`. Evidence is the test file path.
   - `code-read` — grounded in source we can read. Evidence is a `path#Lline` citation in `reference/` or `cli/`.
   - `upstream-doc` — from official upstream documentation. Evidence is a URL or a `reference/.../Documentation/` path.
   - `community` — from a forum, mailing list, or blog. Evidence is a `kb/raw/` file path with the relevant quote.
   - `speculation` — reasoned but unverified. No evidence required, but the claim must be marked explicitly with a local evidence block carrying `Confidence: speculation`.
9. **Optional bottom index** — insights MAY include a closing "Touched code paths" section that lists every code path mentioned in local evidence blocks throughout the document. This is an index for code-change reverse-lookup, not the authoritative evidence structure. The authoritative evidence lives inline in local blocks. Do not duplicate or fork.
10. **The insight↔ADR graduation rule** — when an insight informs a braid decision, the decision graduates to `docs/decisions/` and cites the insight in an `## Evidence` section. Insight stays in `kb/`. ADR template change is deferred but the rule is documented now.
11. **Cross-linking convention** — relative markdown links (e.g. `../../insights/btrfs/foo.md`), not Obsidian-style `[[wikilinks]]`, to stay consistent with the rest of the repo and work with plain `gh` rendering.

The frontmatter spec — coarse document-wide metadata only, no `sources:`:

```yaml
# kb/insights/.../*.md
---
intent: One-line description of what this documents and when to read it.
purpose: decision-evidence | documentation
type: synthesis | empirical
status: draft | active | contested | superseded
confidence: hardware-verified | vm-verified | code-read | upstream-doc | community | speculation   # dominant level — informational; does NOT substitute for claim-level evidence blocks
last_validated: 2026-04-08   # OPTIONAL document-wide date. Omit when unknown. Claim-level dates live inside local evidence blocks.
applies_to:                  # OPTIONAL
  btrfs-progs: ">=6.6"
cited_by:                    # OPTIONAL — populated when an ADR cites this insight
  - docs/decisions/cycle-mount-after-replace.md
---
```

Reverse citations to code/tests/raw/upstream-docs do **not** appear in frontmatter. They live in local evidence blocks adjacent to the claims they support (see section 6).

```yaml
# kb/raw/.../*.md
---
source_url: https://lore.kernel.org/linux-btrfs/...
source_type: mailing-list | blog | forum | web-clipping | dmesg-capture | tool-output
fetched_date: 2026-04-08
participants:                # optional
  - alice@example.org
tool_versions:               # optional, if mentioned in source
  btrfs-progs: "6.7"
---
```

### Step 2: Dogfood — real migration of `sata-hot-unplug.md`

Why this single doc: it's empirical, hardware-verified, has a single coherent topic (SATA hot-unplug behavior on real hardware), and naturally mixes confidence levels (hardware-verified body + speculative "Unanswered Questions" section). It exercises both document-level confidence and the new claim-level override syntax in one file. It does *not* need a `kb/raw/` entry — see "no raw" below.

Why not the synthesis pair (`device-disappearance.md`): we considered migrating both as a pair but rejected it. Migrating the synthesis doc requires touching the state table (which mixes confidence levels and would exercise the new `Evidence` column convention), which is more schema surface than Phase 1 should commit to before validating the basics. The synthesis doc migrates in Phase 2 with the rest. The temporary cross-boundary cross-link from `docs/tool-behavior/device-disappearance.md` → `kb/insights/device-states/sata-hot-unplug.md` is acceptable for the interregnum.

Why no `kb/raw/` entry: the dmesg snippets in `docs/real-world/sata-hot-unplug.md` are already extracted and editorialized — selected lines from a `journalctl` output that was not preserved as a discrete artifact. Promoting them to `kb/raw/` would create fake provenance: the supposed raw node would already be derived. Phase 1 keeps the dmesg text inline in the migrated insight as quoted blocks (the same way the source doc has them today). The `kb/raw/` layer waits until a real preserved external artifact comes along.

Files to write:

1. `kb/insights/device-states/sata-hot-unplug.md` — the migrated content from `docs/real-world/sata-hot-unplug.md`. The migration is structural, not just a copy.

   **Frontmatter** (coarse, no `sources:`):
   - `intent:` — preserved from the source doc
   - `purpose: documentation`
   - `type: empirical`
   - `status: active`
   - `confidence: hardware-verified` (the dominant level — informational only)
   - `last_validated:` — **omitted** (the actual test date is unknown and must not be faked from `git log`)

   **Body** — preserve the existing prose, then attach local evidence blocks. Every block has both `Evidence:` (what proves the claim) and (when applicable) `Validated by:` (braid code that depends on the claim) — these are kept distinct, never merged.
   - The existing "Hardware" section becomes the document's hardware-setup anchor. Other evidence blocks reference it by short tag (e.g. `hardware (hunk)`).
   - The "Test: SATA Hot-Unplug → Immediate state" table gets three new columns: `Evidence`, `Validated by`, `Confidence`. Evidence cells are `hardware (hunk)` for observed rows. Validated-by cells cite the relevant braid handler code path (e.g. `cli/src/probe.rs:190-206` for the null-underlying row, `cli/src/parse/btrfs_filesystem_show.rs:116` for the MISSING-path row). Confidence cells are `hardware-verified` for observed rows.
   - The "Test: SATA Hot-Unplug → State after ~5 minutes" subsection gets a section-level evidence block right after the heading: `Evidence: hardware (hunk), passive observation`, `Validated by: <relevant probe paths>`, `Confidence: hardware-verified`.
   - The "Kernel perspective (dmesg)" subsections get section-level evidence blocks: `Evidence: hardware (hunk), inline dmesg snippet`, `Validated by: <relevant kernel-state-handler paths if any>`, `Confidence: hardware-verified`. The dmesg snippets stay inline (no `kb/raw/` entry — see "no fake raw" above).
   - The "Test: SATA Replug → State after replug" table gets the same three-column treatment.
   - The "Recovery path" section gets a section-level block: `Evidence: hardware (hunk)`, `Validated by: <braid recovery code paths>`, `Confidence: hardware-verified`.
   - The "Unanswered Questions" section gets a section-level evidence block: `Evidence: none — open question`, `Confidence: speculation`. `Validated by:` is omitted (no braid code currently depends on these unverified claims). This dogfoods the speculation level.
   - The existing "Validated Code Paths" closing section is renamed to "Touched code paths (index)" and demoted: it becomes an optional reverse-lookup index that aggregates every `Validated by:` path mentioned in local blocks throughout the document, NOT the authoritative evidence structure. The authoritative `Validated by:` lives inline.

   **Cross-link adjustment**: replace the link to `tool-behavior/device-disappearance.md` with `[../../../docs/tool-behavior/device-disappearance.md](../../../docs/tool-behavior/device-disappearance.md)` — the temporary cross-boundary link.

Files to delete:

2. `docs/real-world/sata-hot-unplug.md` — the original. After deletion, `docs/real-world/` is empty.
3. `docs/real-world/` — the now-empty directory.

Files to edit:

4. `docs/index.md` — remove the `## real-world/` section (it had only this one file; the whole subsection goes).
5. `docs/tool-behavior/device-disappearance.md` — update its inbound cross-link from `[real-world/sata-hot-unplug.md](../real-world/sata-hot-unplug.md)` to `[kb/insights/device-states/sata-hot-unplug.md](../../kb/insights/device-states/sata-hot-unplug.md)`. This is the only edit to this file.

After Step 2: there is exactly one authoritative location for the SATA hot-unplug content (`kb/insights/device-states/sata-hot-unplug.md`), `docs/index.md` accurately reflects what's in `docs/`, and the cross-link from the synthesis doc resolves correctly across the boundary.

### Step 3: Stop and evaluate

After Step 2, read `kb/AGENTS.md` and the new insight file together. Check:

- Does the schema actually match what the dogfooded file needed, or did Step 2 reveal a missing field or wrong category?
- Do the local evidence blocks read naturally inline, or do they break the flow of the prose enough to argue for a different syntax?
- Are the table `Evidence`/`Confidence` columns workable, or do they make tables too wide?
- Does grepping a code path (e.g. `cli/src/probe.rs:190-206`) land directly on the claim that path validates, not just on the document?
- Is the boundary table in the schema clear enough that a future agent reading `kb/AGENTS.md` cold can decide where to put a new piece of information?
- Does the temporary cross-boundary cross-link from `docs/tool-behavior/device-disappearance.md` → `kb/insights/device-states/sata-hot-unplug.md` resolve and read clearly?

Iterate on `kb/AGENTS.md` based on findings. Then this plan is done; bulk migration of the remaining insight-shaped files becomes a separate plan.

## Critical files

To be created in Phase 1:

- `kb/AGENTS.md` — schema doc
- `kb/insights/device-states/sata-hot-unplug.md` — migrated from `docs/real-world/sata-hot-unplug.md`

To be deleted in Phase 1:

- `docs/real-world/sata-hot-unplug.md`
- `docs/real-world/` (empty after the file deletion)

To be edited in Phase 1:

- `docs/index.md` — remove the `## real-world/` section
- `docs/tool-behavior/device-disappearance.md` — update one cross-link to point at the new `kb/` location

Will not be touched in Phase 1:

- `docs/tool-behavior/device-disappearance.md` body content (only its one cross-link is edited)
- Other insight-shaped `docs/` files (Phase 2)
- `docs/decisions/*.md` (no Evidence section yet — separate later plan)
- Top-level `AGENTS.md` (no kb pointer added yet, defer until kb has more content)

## Verification

After Step 2, before declaring Phase 1 done:

- **Single source of truth**: `git status` should show `kb/insights/device-states/sata-hot-unplug.md` added, `docs/real-world/sata-hot-unplug.md` deleted, `docs/real-world/` gone, `docs/index.md` modified (removing the real-world section), `docs/tool-behavior/device-disappearance.md` modified (one cross-link). There must be no `docs/real-world/sata-hot-unplug.md` left on disk.
- **No information loss**: diff the body of `kb/insights/device-states/sata-hot-unplug.md` against the deleted source (via `git show HEAD:docs/real-world/sata-hot-unplug.md`). The original prose should be preserved; the differences are additions (frontmatter, local evidence blocks, table `Evidence`/`Confidence` columns, the renamed bottom index, the updated cross-link).
- **Local evidence adjacency**: pick three substantive claims at random from the new file. Each should have an adjacent local evidence block (section-level, table-row, or per-claim). No claim that has specific backing should rely solely on the document-level frontmatter.
- **Evidence vs Validated-by separation**: spot-check three local blocks. `Evidence:` should describe what proves the claim (hardware setup, raw artifact, source line, doc URL); `Validated by:` should list braid code/tests that depend on the claim. They must not be mixed into a single field. A code path appearing as evidence is acceptable only when the claim is *about* that source line itself (a code-read claim), not when the claim is empirical and the code merely consumes the observation.
- **Citation precision**: grep `kb/insights/` for `cli/src/probe.rs:190-206`. The match should land on the specific line/row/section that lists it under `Validated by:` — not just somewhere in a document-wide list. Repeat with at least one other code path (e.g. `cli/src/parse/btrfs_filesystem_show.rs:116`).
- **Mixed-confidence expressed locally**: confirm the "Unanswered Questions" section's `Confidence: speculation` is in a local evidence block at the section, not only inferred from a document-wide convention or a frontmatter field. Confirm `Validated by:` is omitted in that block (nothing depends on speculation).
- **Cross-link resolves both ways**: from `docs/tool-behavior/device-disappearance.md`, the link to the new `kb/` path resolves. From the new insight file, the link back to `device-disappearance.md` resolves.
- **Schema cold-read**: read `kb/AGENTS.md` cold. If a new agent had only this file and no prior conversation, would they know where to put a new dmesg capture from a future hardware test? a new mailing-list quote? a synthesis claim that contradicts an existing insight? Would they know how to attach evidence to a claim, a table row, and a section? If any answer is unclear, the schema needs more.
- **Index accurate**: `docs/index.md` no longer mentions `real-world/`. Grep `docs/` for `sata-hot-unplug` to confirm only `device-disappearance.md` references it, with the updated path.

## Resolved decisions

- **Citation model is claim-adjacent, not frontmatter-based.** Reverse citations live in local evidence blocks next to the claims they support. Frontmatter is coarse document-wide metadata only and does not contain a `sources:` list. A bottom "Touched code paths" index is optional and is a reverse-lookup convenience, not the authoritative structure.
- **Local evidence block format**: blockquote with two distinct citation fields plus confidence:
  - `Evidence:` — what *proves* the claim is true (hardware setup, `kb/raw/` artifact, `reference/` source line, upstream doc URL, `tests/` test path, or `cli/src/...:Lline` only when the claim is about that source itself).
  - `Validated by:` — what braid code/tests *depend on* the claim (paths under `cli/src/`, `tests/`). Optional; omit when nothing depends on the claim.
  - `Confidence:` — one level from the hierarchy.
  - `Last validated:` — optional, claim-level.
  
  Three placements: section-level (under heading), table columns (`Evidence`/`Validated by`/`Confidence`), per-claim (immediately after the claim). The two fields must never be conflated — a code path that consumes a hardware observation does not validate the truth of the observation.
- **Confidence hierarchy**: six levels — `hardware-verified` → `vm-verified` → `code-read` → `upstream-doc` → `community` → `speculation`.
- **`hardware-verified` evidence rule**: accepts any of (a) a `tests/hw/` test path, (b) an in-body "Hardware" / "Test conditions" section describing machine + setup + method, or (c) a preserved `kb/raw/` capture. The dogfood file uses option (b).
- **`last_validated:` is optional** at both document and claim level, and is the date of the underlying observation or rerun — *not* a `git log` doc-edit date. Omit when the observation date is unknown.
- **Cross-link convention**: relative markdown links (e.g. `[foo](../btrfs/foo.md)`), not Obsidian wikilinks. Consistent with how `docs/` already cross-links and works with plain GitHub rendering.
- **No parallel copies during migration**: every migrated file is removed from `docs/`, the index is updated, and inbound cross-links are rewritten in the same step. Exactly one authoritative location at any time.
- **No fabricated raw provenance**: `kb/raw/` is reserved for preserved original artifacts. Never promote already-derived content to `kb/raw/`.
