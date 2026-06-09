# Plan: capture dev_replace-resume rationale and dissolve the dangling `plans/wip/` citation class

## Context

A `/verify-issue` run on a "missing test coverage" finding established that the
finding's headline was **stale** -- the end-to-end test it claimed was missing
already exists at `tests/repro/btrfs-replace-interrupted-mid-flight.py` and runs
in CI as the `repro-btrfs-replace-interrupted-mid-flight` check
(`flake.nix#checks`). It does exactly what the finding's "Fix" asked for: starts
a real `braid replace`, SIGKILLs mid-flight, reboots, runs `braid recover`, and
asserts the resumed replace drains, `pool.json` swaps in the new disk, the old
disk is evicted, and a follow-up `braid lock; braid unlock` cycle stays clean.

What the finding **did** surface (its salvageable nugget): the load-bearing
empirical rationale for recover's most safety-critical mechanism -- *why the LUKS
close+reopen in the relock-and-remount cycle is load-bearing* -- is cited from
shipping code (`cli/src/recover.rs#relock_and_remount`) and the CI test as
`plans/wip/sharded-drifting-beaver-findings.md`, a file that **does not exist and
was never committed to git**. The rationale is therefore undocumented and the
pointer is dead.

Investigation widened this from one dead pointer into a **3-instance class**
spanning shipping code, tests, and an `Active` ADR: durable artifacts cite
transient `plans/wip/` paths that rot the moment a plan is promoted
(`promote-plan` renames `plans/wip/X.md` -> `plans/impl/<date>-X.md`) or was
never committed at all. braid already lints code comments for stale `docs/`
citations (`scripts/docs/check-code-doc-anchors.py` fails CI on unresolved
`principles.md#anchor` references across `cli/`, `tests/`, `modules/`) but has
**no equivalent guard for `plans/` references** -- and that guard must also cover
`docs/`, because `docs/design/decisions/020-ups-integration.md` (`status:
Active`) cites a transient `plans/wip/` path in its body where no existing check
sees it.

**Intended outcome:** capture the dev_replace-resume rationale in a durable
internals doc, repoint every dangling `plans/wip/` reference to a durable home,
and add a lint so this class cannot recur. This follows braid's charter to "reach
for the ideal, robust, simple, most correct solution -- regardless of scope"
(`AGENTS.md`).

## The dangling-reference class (verified)

| Cited `plans/wip/` path | Status | Cited from | Durable home |
|---|---|---|---|
| `sharded-drifting-beaver-findings.md` | never committed | `cli/src/recover.rs` (1) + `tests/repro/btrfs-replace-interrupted-mid-flight.py` (4) | **new doc** (Phase 1) |
| `cheeky-questing-popcorn.md` | never committed | `cli/src/mount.rs` (1) | existing `docs/internals/luks-unlock.md#messaging-invariant` |
| `forced-shutdown-recovery-proof.md` | promoted to impl/ | `tests/module/ups-lb-during-*.{nix,py}` (5) + `docs/design/decisions/020-ups-integration.md` (1, `Active` ADR body) | `plans/impl/2026-04-21-forced-shutdown-recovery-proof.md` |

Verified via `rg 'plans/wip/\S+\.md'` across `cli/`, `tests/`, `modules/`, and
`docs/` (excl. generated `docs/book/`) plus file-existence checks. The cheeky
target already contains the exact "messages must not reference local
`.luksheader` paths" invariant the comment describes (`docs/internals/luks-unlock.md`,
the `### Messaging invariant` subsection). The forced-shutdown plan exists at the
dated impl path. The ADR 020 citation is a backticked code span in the body --
invisible to `mdbook-linkcheck2` (only `](...)` links) and to `check-see-paths.py`
(only ADR `## See` sections). The one `plans/wip/` mention the guard must spare is
the bare-directory token in `docs/dev/doc-citations.md` (no file component).

## Phase 1 -- Write the durable rationale doc

Create **`docs/internals/btrfs/dev-replace-resume.md`** (an internals
implementation note, not an ADR: it records empirical kernel behavior, not an
architectural decision -- `AGENTS.md` routes implementation notes to
`internals/`, rationale/decisions to `decisions/`).

**Frontmatter (required by `scripts/docs/check-frontmatter.py` over
`docs/internals/**`)** -- match the house form in
`docs/internals/btrfs/enospc-vs-hang.md`:

```
---
intent: "Record btrfs dev_replace resume-on-mount behavior and the recover relock cycle for braid maintainers. Read before changing related behavior or docs."
status: Active
---
# btrfs dev_replace resume-on-mount and the recover relock cycle
```

**Content** (~40-70 lines, sourced from the now-lost note's intended structure
in `plans/impl/2026-04-07-btrfs-replace-interrupted-repro.md` and reconstructed
from commits `21011abc` and `be071517`, the `recover.rs` doc comments, and the
repro test header):

1. **Background** -- a `btrfs replace` interrupted mid-flight by an unclean crash
   leaves the on-disk `dev_replace_item` in STARTED; the kernel resumes on the
   next mount.
2. **Kernel resume-on-mount behavior** -- `btrfs_resume_dev_replace_async` runs
   as a detached kthread that `umount` does **not** wait on. It commits the
   post-completion devid swap to disk correctly but does **not** update the
   in-memory `btrfs_fs_devices` for the mount session that triggered it. A probe
   taken from that session reads stale topology: a phantom `MISSING` devid 0
   plus both source and target devices (five device entries; `braid status`
   reports DEGRADED though all disks are online).
3. **Why the LUKS close+reopen is load-bearing** (the empirical crux the dead
   note held) -- a `umount + btrfs device scan --forget + remount` cycle that
   leaves the dm devices alive does **not** clear the cached `fs_devices`. Only
   tearing down and recreating the dm devices forces the kernel to re-read the
   chunk tree from disk and rebuild a fresh `fs_devices` reflecting the
   post-resume on-disk state.
4. **What braid recover does** -- the two-step guarded by
   `RecoverWorkAction::WaitForKernelReplace` then `RemountCycle`:
   `wait_for_kernel_replace_to_finish` (poll `btrfs replace status` until
   Finished/None; `Running` is intentionally unbounded; `Suspended` or
   unparseable output fails closed and **preserves the journal**), then
   `relock_and_remount` (umount, `scan --forget`, close the LUKS membership
   union, reopen + remount via the standard `plan_open_pool` + execute flow).
   Cite these as code spans (`cli/src/recover.rs#relock_and_remount`,
   `cli/src/recover.rs#wait_for_kernel_replace_to_finish`) per
   `docs/dev/doc-citations.md` -- backticked `path#symbol`, never linkified,
   never line numbers.
5. **Coverage** -- the unclean-kill path is pinned end-to-end by
   `tests/repro/btrfs-replace-interrupted-mid-flight.py`.
6. **Path B: v6.19+ freeze/signal cancellation (not yet covered)** -- orthogonal
   to the unclean-kill path (an unclean kill bypasses the in-loop
   `try_to_freeze`/`fatal_signal_pending` checks entirely). A sibling repro test
   is needed when kernel >= 6.19 reaches NixOS stable; sequencing depends on
   whether `braid replace` should inhibit suspend for the operation's duration.
   This section preserves the meaning of the repro test's line-29 reference,
   which points readers to "the Path B sibling-test sketch."
7. **See also** -- markdown links (validated by `mdbook-linkcheck2`) to
   `docs/commands/recover.md`, `docs/guides/recovery-scenarios.md` (both already
   document the user-facing symptom/workaround -- do not duplicate, cross-link),
   and `docs/design/decisions/012-intent-cli.md`.

**Register in `docs/SUMMARY.md`** under `# Internals`, grouped with the other
`internals/btrfs/` entries (after `luks-sector-size`). Required by `just
check-docs` (SUMMARY.md <-> disk parity). **No `docs/index.md` or `README.md`
edits** -- `scripts/docs/check-doc-tables.py` governs only Guides/Commands
parity, not Internals.

## Phase 2 -- Repoint every dangling reference

In-comment citations to a markdown doc use the plain-path form (no link, no
backticks), matching existing examples like `see
docs/design/decisions/026-pool-lock-rust-owned.md` in `cli/src/main.rs`.

- **sharded (5 occurrences -> new doc):** replace the literal
  `plans/wip/sharded-drifting-beaver-findings.md` with
  `docs/internals/btrfs/dev-replace-resume.md` in `cli/src/recover.rs`
  (`#relock_and_remount` doc comment) and at the four sites in
  `tests/repro/btrfs-replace-interrupted-mid-flight.py` (the file header, the
  Phase-7 preamble, and the two assert messages). Pure path swap; surrounding
  prose unchanged.
- **cheeky (1 -> existing anchor):** in `cli/src/mount.rs#format_degraded_refused_does_not_reference_local_header_backups`,
  repoint `plans/wip/cheeky-questing-popcorn.md` to
  `docs/internals/luks-unlock.md#messaging-invariant`.
- **forced-shutdown (6 -> promoted impl path):** repoint
  `plans/wip/forced-shutdown-recovery-proof.md` to
  `plans/impl/2026-04-21-forced-shutdown-recovery-proof.md` in the five
  `tests/module/ups-lb-during-*` files **and** in the body of
  `docs/design/decisions/020-ups-integration.md` (a backticked code span in the
  blocker-resolution list). The ADR is `status: Active`, so tracking the promoted
  path is a correctness fix, not history-editing (see Scope boundary).

## Phase 3 -- Lint to prevent recurrence

Add **`scripts/docs/check-plans-refs.py`**, structurally mirroring
`scripts/docs/check-code-doc-anchors.py` (same `iter_files` skipping
`target/.git/.direnv/__pycache__`, same `path:line: message` failure format and
exit-1 convention) plus a `--selftest` mode as `check-output-ascii.py` and
`check-doc-links.py` do.

**Scope:** `SEARCH_ROOTS = [cli, tests, modules, docs]`, with `docs/book`
(generated HTML) added to the `iter_files` skip-set. Adding `docs` is required by
F1: `docs/design/decisions/020-ups-integration.md` (an `Active` ADR) cites a
transient `plans/wip/` path that no current check sees. `plans/` itself stays out
of scope so `plans/wip` <-> `plans/impl` cross-references remain free.

**Rule:** forbid references to a specific transient *file* -- match
`plans/wip/<filename>` (a path with a file component, e.g. `plans/wip/x.md`), not
the bare `plans/wip/` directory token. This discriminator is load-bearing:
`docs/dev/doc-citations.md` legitimately discusses "transient analysis in
`plans/wip/`" (directory, no file) and must NOT be flagged. **Allow
`plans/impl/`** (committed, dated, stable provenance), which keeps the repointed
forced-shutdown citations valid. The failure message names the durable
alternatives: "cite a `docs/` page or the promoted `plans/impl/<date>-*.md` path
instead." Run this lint last (after Phase 2) so it passes.

**Selftest fixtures** must lock the discriminator, not just the happy path:
`plans/wip/x.md` -> flagged; bare `plans/wip/` token -> not flagged;
`plans/impl/2026-01-01-x.md` -> not flagged. Drive them through the same scan
function the live run uses (as `check-doc-links.py`'s `_selftest` does).

**Wire into the harness** mirroring `check-output-ascii` / `check-doc-links`,
which run `--selftest` *before* the live scan in **both** places:
- `justfile`: add a `check-plans-refs:` recipe with two command lines --
  `python3 scripts/docs/check-plans-refs.py --selftest` then
  `python3 scripts/docs/check-plans-refs.py` -- alongside `check-code-doc-anchors`
  / `check-docs-see-paths`.
- `.github/workflows/checks.yml`: add a `plans-refs` job whose step is a
  `run: |` block running the selftest line then the live-scan line (the exact
  shape of the existing `output-ascii` / `doc-links` jobs), with the other
  pure-Python doc checks (no nix dependency).

## Phase 3b -- Generalize the doc-anchor lint to guard the cheeky repoint target

The cheeky repoint (Phase 2) trades a transient `plans/wip/` path for a
heading-slug target, `docs/internals/luks-unlock.md#messaging-invariant`, cited
from a `cli/src/mount.rs` comment. **Nothing validates that anchor today:**
`mdbook-linkcheck2` only resolves `](...)` links *inside* the book tree (this
citation is plain-path, in a `.rs` file outside it), `check-see-paths.py` covers
only ADR `## See` sections, `check-plans-refs.py` forbids only `plans/wip/`, and
`check-code-doc-anchors.py`'s `CITE_PATTERN` is hardcoded to
`docs/design/principles\.md#`. Repointing cheeky would otherwise swap a
soon-to-be-guarded transient ref for an unguarded durable one -- the same rot one
class over, on the next heading rename.

Close it by **generalizing `scripts/docs/check-code-doc-anchors.py`** from
principles-only to any `docs/**.md#anchor` citation:
- Broaden `CITE_PATTERN` to capture `(docs/<path>.md)#(anchor)`; for each hit,
  resolve `<path>.md` under the resolution root (the module `ROOT` live;
  parameterized for the selftest -- next bullet), fail if the file is missing,
  else check the anchor against `anchors_of(target)` (the `_mdslug` helper it
  already imports). Cache `anchors_of` per file.
- Extract the per-file scan into a shared entry point, as `check-doc-links.py`
  factors out `lint_file` ("the single entry point shared by the live scan and
  `--selftest`"), so both paths exercise the same code. The current `main()`
  scans inline against one `anchors_of(PRINCIPLES)`; the per-target
  generalization rewrites that inline body anyway, so the extraction is free.
  **Thread the resolution root through this entry point as an explicit parameter**
  (not the module-global `ROOT`): live `main()` passes `ROOT`, the selftest passes
  its `TemporaryDirectory` root. This is the one place the `check-doc-links.py`
  analogy must be adapted, not copied: its `lint_file` resolves each target
  relative to the *scanned file's own parent* (`resolved = md_path.parent / path`,
  `check-doc-links.py:86`), so a temp file beside a temp dep is hermetic with no
  root parameter. This lint resolves a cited `docs/...` against the *repo root* (a
  `docs/...` citation in `cli/src/mount.rs` points at repo-root `docs/`, not
  `cli/src/docs/`), so the temp tree only becomes the resolution base when the
  root is passed in.
- Add a `--selftest` mode (a hermetic temp-tree `_selftest` like
  `check-doc-links.py`'s, driving that shared scan function with the temp root
  passed in per the previous bullet, so every fixture citation and its target doc
  resolve inside the tree -- never against live `luks-unlock.md` headings) whose
  fixtures lock the broadened contract -- above all that the pattern matches the
  **plain-path**
  form (a bare `docs/...#anchor` in a code comment, as cheeky's now is), not only
  the `](...)` markdown-link form. Fixtures: plain-path citation to a real
  heading -> not flagged; plain-path citation to a missing heading -> flagged;
  citation to a missing file (`docs/nope.md#x`) -> flagged; a `principles.md#`
  citation to a real anchor -> still resolves (regression guard for the original
  behavior). This selftest is load-bearing precisely because the clean live tree
  cannot surface the gap: the only pre-existing non-principles citations are the
  three `AGENTS.md` *markdown links*, so a regex that silently matched only the
  link form -- or a future revert to principles-only -- would keep CI green while
  leaving cheeky's plain-path citation unguarded. (It is also the asymmetry the
  reviewer flagged: Phase 3's new `check-plans-refs.py` gets a selftest; the
  behavior-broadening generalization here must get one too.)
- Keep its `SEARCH_ROOTS` and name -- the `code-doc-anchors` CI job and
  `check-code-doc-anchors` justfile recipe keep their identity. The **one** wiring
  change (correcting the earlier "no wiring change" note): run `--selftest` before
  the live scan in both, matching the `output-ascii` / `doc-links` / `plans-refs`
  selftest-then-live shape -- the recipe gains a `--selftest` line above its live
  line, and the CI job's single `run:` becomes a `run: |` block of the two lines.

Blast radius is small and safe: the only existing non-principles `docs/**#anchor`
citations in those roots are three in `AGENTS.md`, all `](...)` markdown links
already validated (CI-green) by `check-doc-links.py`, so the generalization
surfaces no new failures -- it only newly covers plain-path code-comment
citations like cheeky's. Run it after Phase 2 to confirm green.

## Scope boundary (intentionally out)

`plans/impl/` historical records that reference the never-written sharded note
(e.g. `plans/impl/2026-04-07-btrfs-replace-interrupted-repro.md`,
`plans/impl/2026-04-07-mount-credential-plan-resolve-execute.md`) stay **as-is**.
They are frozen point-in-time records describing an intended-but-dropped
deliverable; per `docs/dev/doc-citations.md`, frozen plan/decision bodies are not
rewritten to track current code. The lint scans `cli/`, `tests/`, `modules/`,
`docs/` (excl. generated `docs/book/`) -- not `plans/` -- so these records are
not flagged.

Repointing `docs/design/decisions/020-ups-integration.md` does **not** violate
that boundary: it is `status: Active`, not a frozen (`Superseded`/`Deprecated`)
ADR, so tracking the promoted impl path is correct, not history-rewriting. The
`(preserved in git history; last present at <hash>)` form applies only to frozen
docs whose referenced file has no live successor -- here the successor exists at
the dated impl path, so a plain repoint is right.

## Verification

- `python3 scripts/docs/check-frontmatter.py` -> new doc frontmatter valid.
- `just check-docs` -> SUMMARY.md parity (new internals page registered).
- `just docs-build` -> `mdbook-linkcheck2` validates the new doc's *in-book*
  outbound links/anchors (e.g. `recover.md#what-happens-under-the-hood` and the
  recovery-scenarios anchor); a broken in-book link fails the build. It does NOT
  see the `#messaging-invariant` citation -- that lives in `cli/src/mount.rs`,
  outside the book, and is guarded by Phase 3b instead.
- `python3 scripts/docs/check-plans-refs.py --selftest` then the live scan ->
  both pass; the selftest proves the scanner flags `plans/wip/x.md`, spares the
  bare `plans/wip/` token, and allows `plans/impl/<date>-x.md`.
- `rg 'plans/wip/\S+\.md' --glob '!plans/**' --glob '!docs/book/**'` -> **no
  matches** (every specific transient-file citation -- code, tests, and ADR 020 --
  repointed; the bare-directory mention in `docs/dev/doc-citations.md` remains and
  is intentionally allowed).
- `python3 scripts/docs/check-code-doc-anchors.py --selftest` then the live scan
  (generalized in Phase 3b) -> both pass. The selftest proves the broadened
  scanner matches the plain-path form (not only `](...)` links), flags a bad
  anchor and a missing file, and still resolves the `principles.md#` form; the
  live run now resolves the `docs/internals/luks-unlock.md#messaging-invariant`
  citation in `cli/src/mount.rs` plus the three existing `AGENTS.md` doc anchors.
- `python3 scripts/docs/check-see-paths.py` -> still green (unaffected).
- The repro test is unchanged behaviorally (only a comment path moved), so a full
  VM rerun is optional; if run: `nix build .#checks.<system>.repro-btrfs-replace-interrupted-mid-flight`.

## Critical files

- **New:** `docs/internals/btrfs/dev-replace-resume.md`,
  `scripts/docs/check-plans-refs.py`
- **Edit:** `docs/SUMMARY.md`, `cli/src/recover.rs`, `cli/src/mount.rs`,
  `tests/repro/btrfs-replace-interrupted-mid-flight.py`,
  `tests/module/ups-lb-during-*.{nix,py}` (5 files),
  `docs/design/decisions/020-ups-integration.md`,
  `scripts/docs/check-code-doc-anchors.py` (generalized + `--selftest`, Phase 3b),
  `justfile`, `.github/workflows/checks.yml`
- **Reused (repoint targets, not edited):**
  `docs/internals/luks-unlock.md#messaging-invariant`,
  `plans/impl/2026-04-21-forced-shutdown-recovery-proof.md`
