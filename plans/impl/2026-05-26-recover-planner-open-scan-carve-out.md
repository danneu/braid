# Document recover's planner-phase open/scan carve-out

## Context

An `/ultrareview` finding (Low / project-fit) flagged that `plan_recover`
performs real mutations -- LUKS opens (`luks::ensure_luks_open`) and a btrfs
device scan (`scan_mapper_if_btrfs_visible`) -- inside the planner via
`discover_add_targets_before_mount` (`cli/src/recover.rs:1967-2054`, called at
`1246-1259`, gated by `!params.dry_run`). Decision 022 frames `plan_*()` as the
read-side/preview boundary, and every sibling planner honors that:
`mount::plan_open_pool` is documented "No mutations -- safe for dry-run"
(`cli/src/mount.rs:172-181`), and `plan_add`/`plan_replace`/`plan_remove`/
`plan_remove_missing` all defer LUKS opens to `execute` (e.g. replace's
`ensure_luks_open` is at `replace.rs:671/742`, inside `ReplacePlan::execute`).
Recover is the **lone** planner-phase mutation.

The code is correct and the mutation belongs in the planner -- it is not a bug.
But the carve-out is undocumented in two ways a future maintainer would hit:

- `discover_add_targets_before_mount` has **no `///` doc comment**, violating
  the project's doc-comment convention for a non-obvious boundary function.
  Worse, the *adjacent* dry-run reconciliation block (`recover.rs:1311-1357`)
  carries a thorough comment, so the surprising mutation is the one piece left
  unexplained.
- Decision 022 records the read-side principle but not recover's intentional
  exception, so the code silently bends a documented principle. AGENTS.md's
  architecture-authority rule says principle-bending code must carry its
  rationale in the decision doc.

This change documents the exception in both places. **No behavior changes.**

### Why a code refactor is not the fix

Moving the open/scan into `execute` was considered and rejected -- it would
break two invariants the planner-phase placement exists to satisfy:

1. **Single passphrase (Principle 4).** The credential is resolved once in the
   preflight/planning window (where an interactive TTY prompt belongs) and
   cached in `RecoverWorkPlan.pre_resolved_credential` (`recover.rs:1519`), then
   reused by execute's discovery and replay paths via `recover_passphrase`
   without re-prompting (commits `1638df2`, `f597363`).
2. **btrfs visibility before the initial mount.** For the pool-not-mounted
   case, scanning a committed-but-closed add-target registers it in the kernel
   before execute's `InitialOpenPool` mount, so the mount assembles the
   already-committed device instead of recover re-adding or re-formatting it
   (plan `plans/impl/2026-05-05-resumable-existing-pool-add-transaction.md:181-205`).

Note `mount_membership_for_recover` returns `pre_membership` (which *excludes*
the add-targets) for non-bootstrap Add PoolMutation (`recover.rs:3623-3636`), so
this is a distinct preflight step, not work `plan_open_pool` could absorb.

## Change

Three edits, comment/doc-only. Match the existing style: plain backticks in doc
comments (not rustdoc intra-doc `[]` links), consistent with `mount.rs:165-181`.

### 1. `///` doc comment on `discover_add_targets_before_mount` (primary)

`cli/src/recover.rs:1967`. Per the repo doc-comment convention (CLAUDE.md:
"Prefer one to three lines"), keep this terse: state the intent, flag the
deliberate `!dry_run`-gated exception, and point to Decision 022 for the full
rationale (edit 3 carries the two reasons). Word it as *may* open/scan the
*validated* targets -- not all journaled targets -- and tie credential
resolution to opening a closed target, since already-open/skipped targets never
prompt (`recover.rs:2026`; edit 3 states the exact conditions). Draft:

```rust
/// Preflight for an interrupted existing-pool add: may open closed validated
/// add-targets -- resolving the unlock credential once when it does -- and scan
/// them before mount. The deliberate `!dry_run`-gated exception to Decision 022.
```

### 2. Brief call-site comment (symmetry / discoverability)

`cli/src/recover.rs:1246`, immediately above the `if let journal::OpKind::Add
{ .. } && !params.dry_run` block. Matches the commenting density of the
dry-run reconciliation block just below it (`1311+`). Draft:

```rust
// Preflight add-target reconciliation -- the one deliberate mutation in the
// planner. Gated by `!dry_run` so preview stays side-effect-free; see
// `discover_add_targets_before_mount` for why it must precede the mount.
```

### 3. Decision 022 -- the full rationale (architecture authority)

`docs/design/decisions/022-dry-run-preview-model.md`, in the **Scope** section,
right after the precedent sentence (line 96, "... `remove-missing`, and
`recover`."). This is where the doc already carries command-specific notes
(lock's typed close set; remove/replace confirmation specifics). Because edit 1
keeps the `///` terse, this paragraph carries the detailed two-reason rationale,
and states the conditions precisely -- the open, scan, and credential prompt are
each conditional, not blanket. Draft:

```markdown
Recover is the one deliberate exception to the read-side planner rule. When
recovering an interrupted existing-pool add and the pool is not already mounted,
`plan_recover` reconciles the validated add-targets -- those present,
LUKS-openable, and not yet pool members -- before mount: it opens any whose
mapper is closed (resolving the unlock credential once, and only then), and
btrfs-scans a target only when its mapper shows a btrfs signature. All of this
is gated by `!dry_run` (`discover_add_targets_before_mount`, after an
already-mounted short-circuit). The preflight is non-destructive and exists for
two reasons: resolving the credential in the preflight window where an
interactive prompt belongs, then caching it so execute reuses it without a
second prompt (single passphrase, Principle 4); and making an
already-committed-but-closed target visible to the kernel before the initial
mount so the mount assembles it instead of recover re-adding or re-formatting
it. It is not a general license to mutate inside `plan_*()`.
```

## Files to modify

- `cli/src/recover.rs` -- add the `///` doc comment (edit 1) and the call-site
  comment (edit 2).
- `docs/design/decisions/022-dry-run-preview-model.md` -- add the Scope sentence
  (edit 3).

## Out of scope

- No behavior change: do not move, gate differently, or refactor
  `discover_add_targets_before_mount` or its call site.
- Do not run any formatter (`cargo fmt` / `just fmt`) -- make the edits by hand.

## Verification

This is a comment/doc-only change; no tests change behavior.

1. `just test-rust` (or `cargo check -p braid-cli`) -- confirms the doc comment
   parses and nothing in `recover.rs` broke. No new doc tests are introduced.
2. `nix develop .#docs -c mdbook build docs` -- the `docs` devShell
   (`flake.nix`, `docsShellFor`) provides `mdbook` + `mdbook-linkcheck` +
   `mdbook-yml-header`, which `book.toml` requires; a bare `mdbook build docs`
   relies on host PATH and can skip those preprocessors. Confirms Decision 022
   still builds and linkcheck passes (the added prose introduces no new
   cross-links, but the build is the project's gate for doc edits).
3. Read-through: confirm the comment + Decision 022 claims still match the code
   -- the mount-gated short-circuit (`recover.rs:1974-1979`), per-target
   UUID/label guards (`2003-2023`), credential resolution sitting inside the
   `if !mapper_open` open path so already-open/skipped targets never prompt
   (`recover.rs:2026-2036`), and the credential caching into
   `pre_resolved_credential` (`1519`).
4. No VM tests required (no runtime behavior changes).
