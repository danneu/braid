# Plan: project-wide `Preview` for `--dry-run`

## Context

Today every `--dry-run` command improvises its own stdout/stderr split. `lock` aggregates the full preview into one stdout `String` (warnings folded inline). The other seven (`unlock`, `recover`, `add`, `enroll`, `remove`, `remove-missing`, `replace`) leak probe events, discovery lines, pre-plan warnings, or entry banners onto stderr -- some of which change how the user should interpret the preview.

The project-wide rule going forward:

- Successful `--dry-run` = print exactly one rendered `Preview` to stdout. stderr is empty on the success path.
- Preview-generation failure = return `Err`, print to stderr, exit nonzero.
- Anything that affects interpretation of the preview is a `PreviewNote`, not stray stderr.
- No-op is expressed inside `Preview` (zero steps + `Info` note), not special-cased per command.
- Real execution keeps stderr for progress/warnings/confirmations. Plan-derived `PreviewNote::Warn` uses the same canonical `[warn]  <body>` rendering in both dry-run and real-run; execution-only warnings and confirmations stay command-local.

`lock` is the shape; this plan generalizes it across all eight dry-run commands via a new `Preview` type and a plan-object layering (`plan_*()` -> `XxxPlan`; `plan.preview() -> Preview`; `plan.execute(...) -> Result<(), XxxError>`).

## Design

### New module: `cli/src/preview.rs`

```rust
pub struct Preview {
    pub completeness: PreviewCompleteness,
    pub notes: Vec<PreviewNote>,
    pub steps: Vec<Step>,   // reuse existing Step from cmd.rs
}

pub enum PreviewCompleteness {
    Complete,
    Partial { reasons: Vec<PreviewGap> },
}

#[serde(tag = "reason", content = "detail")]
pub enum PreviewGap { /* empty for now; first variant added alongside first use */ }

pub enum PreviewNote {
    Info(String),
    Warn(String),
    PerDisk { name: String, level: NoteLevel, message: String },
}

pub enum NoteLevel { Ok, Skip, Warn, Error }

impl Preview {
    pub fn render(&self) -> String { /* see render order */ }
    pub fn print(&self) { print!("{}", self.render()) }
}
```

All five types `#[derive(Debug, Clone, Serialize)]`. No `Deserialize` (asymmetric by intent -- we emit, never ingest). Keep `PreviewGap` tagged from day one so later variants are non-breaking JSON.

`Step` stays in `cmd.rs` unchanged; `preview.rs` imports it.

**Render order (pinned contract):**

1. `notes` in **insertion order** -- no grouping by kind. This is load-bearing for `unlock` and `recover`, whose current output interleaves `AlreadyMounted`/per-disk/entry-banner in a specific command-chosen order. `Preview::render` does not reorder; each `plan_*` controls ordering by how it appends. Per-disk lines render via `render_per_disk_notes` in `Bracketed` style; `Info` prints unadorned; `Warn` prints `[warn]  <msg>`. **`PreviewNote::Warn` body carries the warning text only -- no `warning:` / `WARNING:` prefix.** The renderer owns the `[warn]  ` marker; legacy prefixed strings are stripped when constructing the note (so a current `eprintln!("warning: pool has N missing device(s)")` becomes `PreviewNote::Warn("pool has N missing device(s)".into())`, rendering as `[warn]  pool has N missing device(s)`). Real-run stderr for plan-derived warnings reuses the same renderer so success, failure, and dry-run share one warning contract; execution-only warnings outside `Preview` remain command-local.
2. Steps rendered via the existing `Step::render_dry_run`; when `steps.is_empty()` and `notes` do not already signal a no-op (via an `Info` note), the literal `nothing to do.\n` is emitted (preserves `lock`'s current contract).
3. If `completeness == Partial`, a final `note: preview incomplete -- <reason>` line per `PreviewGap`.

Per-command note ordering:

- `unlock` / `recover`: notes appended exactly in the order `mount::render_probe_events` produces them today (`AlreadyMounted` before per-disk lines when it appears, otherwise per-disk in membership iteration order). `recover` additionally prepends its entry-banner `Info` note before probing, matching today's stderr sequence.
- `add` / `replace`: warning notes appended in their current `eprintln!` order (missing-devices before keyfile-asymmetry for `add`).
- `enroll`: per-disk skip notes appended in membership iteration order, identical to today.
- `lock` / `remove` / `remove-missing`: any `Warn` note is prepended before steps (matches today's warn-above-steps layout).

### Plan-object layering (pinned)

Commands that accumulate notes before validating (`unlock`, `recover`, `add`, `enroll`) must preserve accumulated context notes even when planning fails -- this mirrors the existing `mount::plan_open_pool` -> `PlanReport { events, result }` contract (`mount.rs:156-159`, pinned by `mount.rs:1868`). Users see per-disk probe context before a refusal (e.g. `DegradedRefused`).

**Note ownership (pinned to avoid double-source drift):**

- `XxxPlan.notes` -- the single source of truth for notes that must appear on **successful** preview *and* successful real-run. Both `preview()` and `execute()` read from here. `execute()` is responsible for rendering `self.notes` to stderr at the top of its body (before mutations), preserving today's "probe context then work" real-run sequence.
- `XxxPlanReport.notes` -- **failure-path only**. Populated only when `plan_*` accumulated context and then bailed (e.g. `DegradedRefused` after per-disk probing). On the `Ok` branch, `XxxPlanReport.notes` is empty; all accumulated notes have been moved into `plan.notes`.

Two planning return shapes. Pick per command based on whether it accumulates notes before validation:

**Shape A -- notes-carrying report (for `unlock`, `recover`, `add`, `remove`, `remove-missing`, `enroll`):**

```rust
pub struct XxxPlanReport {
    pub notes: Vec<PreviewNote>,     // populated whether planning succeeds or fails
    pub result: Result<XxxPlan, XxxError>,
}

pub fn plan_xxx(runner, fs, config, params...) -> XxxPlanReport { ... }
```

**Shape B -- plain `Result` (for `lock`):**

```rust
pub fn plan_xxx(...) -> Result<XxxPlan, XxxError> { ... }
```

`lock`'s orphan-scan warn is already a preview note inside the successful preview; its pre-plan probing cannot fail mid-accumulation (the orphan-scan warn is the only note and it's recoverable). `replace` also uses Shape B because its migrated dry-run preview has zero notes and the remaining warnings stay confirmation-only inside `execute()`. No notes-carrying report is needed for either command.

Every plan exposes:

```rust
pub struct XxxPlan { pub notes, pub steps, /* execute inputs */ }

impl XxxPlan {
    pub fn preview(&self) -> Preview { /* build Preview from notes + steps */ }
    pub fn execute(self, runner, fs, cred, progress...) -> Result<(), XxxError> { ... }
}
```

`cmd_xxx` for Shape A:

```rust
let report = plan_xxx(...);
let plan = match report.result {
    Ok(p) => p,
    Err(e) => {
        // Failure-path: render report.notes (the failure context that was
        // accumulated before the planning error) to stderr, then the error.
        // STYLE is per-command:
        //   - enroll uses PerDiskStyle::Plain  (matches today's pre-passphrase `skip:` lines;
        //     post-passphrase `ok:`/`enroll:` lines stay as direct eprintln! in plan_enrollment)
        //   - unlock/recover/add/remove/remove-missing use Bracketed
        //     (matches today's `[ok  ]  disk: X    ...` from mount::render_probe_events)
        eprint!("{}", preview::render_notes_for_stderr(&report.notes, XXX_STYLE));
        return Err(e);
    }
};
// plan.notes is the single source of truth for successful-path notes.
if params.dry_run {
    plan.preview().print();   // Preview::render always uses Bracketed
    return Ok(());
}
// Real-run: plan.execute(...) renders plan.notes to stderr at the top of
// its body (before mutations) using the SAME per-command style the
// failure path uses. The style choice is uniform across success/failure
// paths per command, so implementers cannot accidentally route one path
// to Plain and the other to Bracketed.
plan.execute(...)
```

Make `XxxPlan` carry the style: add `pub const STDERR_STYLE: PerDiskStyle` as an associated const on each plan type, or a method `fn stderr_style(&self) -> PerDiskStyle`. Both `execute()` and the `cmd_*`-level failure template read the same constant. The contract the plan pins is "success and failure paths MUST use the same per-command style". In practice only `enroll` uses `Plain`; all others use `Bracketed`.

`cmd_xxx` for Shape B:

```rust
let plan = plan_xxx(...)?;
if params.dry_run {
    plan.preview().print();
    return Ok(());
}
plan.execute(...)
```

**Failure-path stderr contract:** any accumulated notes render to stderr before the error message. This mirrors today's `mount::print_probe_events(&report.events); let plan = report.result?;` pattern (`unlock.rs:47-48`, `recover.rs:177-178`) and keeps `mount.rs:1868`'s byte-level contract intact -- the notes rendering on the Err path uses the same `preview::render_notes_*` helper as on the Ok path.

**No shared `CommandPlan` trait.** Each command's `execute` has a unique signature (errors, credentials, progress, inhibitor). A one-method trait `fn preview(&self)` adds crate-level indirection for zero polymorphism and is a YAGNI today. Revisit only if `--format json` wants a generic entry point.

## Per-command migration recipes

Each command keeps its existing mutation code inside `execute`; what moves into `plan_*` is everything currently above the `if dry_run` gate, plus the `eprintln!` warnings/events that change how a user interprets the preview.

- **`lock`** (pattern-setter, Shape B). `LockPlan { notes, steps }`. `plan_lock` owns mountpoint probe + `scan_orphan_mappers` + `compile_lock_steps`. Orphan-scan failure -> `PreviewNote::Warn(orphan_scan_warn_body(e))` (mechanical port of `render_lock_dry_run`). Real-run path at `lock.rs:261` keeps `eprintln!("[warn]  {}", orphan_scan_warn_body(e))` verbatim.

- **`unlock`** (Shape A). `plan_unlock -> UnlockPlanReport { notes, result: Result<UnlockPlan, UnlockError> }`. `UnlockPlan { notes, steps, open_plan: Option<OpenPlan> }`. `plan_unlock` calls `mount::plan_open_pool`, converts each `ProbeEvent` via `to_preview_note()` (see shared helpers), and places the resulting notes on `plan.notes` (success) or `report.notes` (failure). No raw `probe_events` field on the plan -- `plan.notes` is the single rendering source for both dry-run preview and real-run stderr. The unconditional `mount::print_probe_events(&report.events)` call at `unlock.rs:47` is replaced by `execute`'s top-of-body `eprint!(render_notes_for_stderr(&self.notes, PerDiskStyle::Bracketed))`.

- **`recover`** (Shape A). `plan_recover -> RecoverPlanReport { notes, result: Result<RecoverPlan, RecoverError> }`. `RecoverPlan { notes, steps, open_plan, journal, ... }` -- no raw `probe_events` field; notes are the single render source. `plan_recover` wraps the journal load + `union_memberships` + `plan_open_pool` + the already-mounted reconciliation loop (`recover.rs:186-207`). The entry line at `recover.rs:158-162` is extracted to `format_recover_entry(journal) -> String`: dry-run adds `PreviewNote::Info(format_recover_entry(&journal))` as the first note; real-run does `eprintln!("{}", format_recover_entry(&journal))` at the top of `execute`. The bootstrap-advice `Err` at `recover.rs:303-316` stays a preview-generation failure (stderr, exit 1). The already-mounted reconciliation stays a preview-only build step (dry-run semantics today), matching current behavior -- do not silently widen to real-run.

- **`add`** (Shape A). `plan_add -> AddPlanReport { notes, result: Result<AddPlan, AddError> }`. `AddPlan { notes, steps, names, by_ids, probed, pool, ... }`. `plan_add` owns the block `add.rs:320-390`. Missing-devices warning (`:348-354`) -> `PreviewNote::Warn(body)` (body-only, no `warning:` prefix); keyfile-asymmetry warning (`:365-371`) -> `PreviewNote::Warn(format_add_keyfile_asymmetry_warning())` (body-only, no `WARNING:` prefix). The "already in pool" no-op at `:399` -> zero steps + `PreviewNote::Info(format_add_noop(label))`; real-run keeps the same formatter for the no-op line. `AddPlan::execute` and `cmd_add`'s preserved-context failure path both render notes through `preview::render_notes_for_stderr`, so dry-run stdout, real-run stderr, and failure-path stderr all use the same canonical `[warn]  <body>` contract for plan-derived warnings.

- **`enroll`** (Shape A, narrow scope). `plan_enroll -> EnrollPlanReport { notes, result: Result<EnrollPlan, EnrollError> }`. `EnrollPlan { notes, steps, candidates, ... }`.

  **Scope:** this migration only converts the pre-passphrase discovery branch (`enroll_key_file.rs:55-83`) into notes. The post-passphrase planning lines at `:155` (`ok: <name> -- keyfile already enrolled`) and `:168` (`enroll: <name> -- will add keyfile to slot 1`) stay as `eprintln!`s inside `plan_enrollment`. Sequencing reason: these lines are emitted by `plan_enrollment`, which requires the passphrase to be already resolved (e.g. `verify_key_file` at `:153`). Moving them into `plan.notes` rendered at `execute`'s top-of-body would either require impossible pre-passphrase precomputation, or change the wrong-passphrase UX by leaking status lines before the error. Leaving them as real-run-only `eprintln!`s preserves today's sequencing. They do not appear in today's dry-run (`plan_enrollment` is bypassed in `:322-329` and `:382-391`), and do not appear in the migrated dry-run either.

  **Discovery-note mapping** (the only notes that move):
  - `skip: <name> not present` (`:65`) -> `PerDisk { level: Skip, name, message: "not present" }`
  - `skip: <name> not LUKS-formatted` (`:68`) -> `PerDisk { level: Skip, name, message: "not LUKS-formatted" }`

  `PerDiskStyle::Plain` renders `<tag>: <name> <message>` (no inserted delimiter). `Plain` tag mapping stays simple: `Ok` -> `ok`, `Skip` -> `skip`, `Warn` -> `warn`, `Error` -> `error`. No `Action` variant needed; `NoteLevel` stays `{ Ok, Skip, Warn, Error }`.

  **No-candidates is a preserved-context failure.** `discover_enrollment_candidates` accumulates skip notes for every absent / non-LUKS member, then may return `Err(EnrollKeyFileError::Validation("no present LUKS disks found..."))` at `:76-79`. Under Shape A, those notes go to `report.notes` on the `Err` branch, and `cmd_enroll_key_file` renders them via `PerDiskStyle::Plain` before the error -- preserving today's `skip: X not present\nskip: Y not present\nno present LUKS disks found...` stderr shape.

  **Real-run sequencing** (preserved wording-for-wording):
  1. `plan_enroll` -> `EnrollPlanReport`. On `Err`: print `report.notes` (Plain) to stderr, then the error. Done.
  2. On `Ok`: `cmd_enroll_key_file` prints `plan.notes` (Plain) to stderr -- the same skip lines users see today pre-passphrase.
  3. Read passphrase (`:334` / `:393`).
  4. Call `plan_enrollment` (`:338` / `:394`). Today's `eprintln!("ok: ...")` at `:155` and `eprintln!("enroll: ...")` at `:168` remain in place unchanged -- they emit post-passphrase, post-preflight, real-run only.
  5. `generate_key_file` / `apply_enrollment`.

  Dry-run shows only the discovery skip notes in the preview (via `Bracketed`), then the steps from `compile_enroll_steps` -- same as today's dry-run output modulo the style change (bracketed vs plain). Dry-run does not, and cannot, show the `ok:` / `enroll:` lines; today's dry-run also does not.

- **`remove`** (Shape A). `plan_remove -> RemovePlanReport { notes, result: Result<RemovePlan, RemoveError> }`. `RemovePlan { notes, steps, name, target_devid, ... }`. `plan_remove` owns probe + `check_eviction_space` + `compile_remove_present_steps`. ENOSPC soft-warn `eprintln!` (`remove.rs` pre-flight) -> `PreviewNote::Warn`. `RemovePlan::execute` renders plan-derived warnings through `preview::render_notes_for_stderr`, so dry-run stdout and real-run stderr both use the canonical `[warn]  <body>` form. Confirmation dialog at `:149-162` stays in `execute` (real-run only) -- `--dry-run` never sees it now.

- **`remove-missing`** (Shape A). `plan_remove_missing -> RemoveMissingPlanReport { notes, result: Result<RemoveMissingPlan, RemoveMissingError> }`. `RemoveMissingPlan { notes, steps, missing_id, will_clear_last_missing, ... }`. `plan_remove_missing` owns `:100-150`. ENOSPC pre-flight warnings (`:260,:268`) -> `PreviewNote::Warn`. `RemoveMissingPlan::execute` renders plan-derived warnings through `preview::render_notes_for_stderr`, so dry-run stdout and real-run stderr both use the canonical `[warn]  <body>` form. The `eprintln!` at `:213-216` is real-run progress only -- lives in `execute`. Confirmation at `:166-175` moves into `execute`.

- **`replace`** (Shape B). `plan_replace -> Result<ReplacePlan, ReplaceError>`. `ReplacePlan { steps, config, new_name, new_by_id, pool, replace_source, new_probed, pre_membership, target_membership }`. `plan_replace` owns everything above today's `if params.dry_run` gate: membership + probe + `compile_replace_steps`. `ReplacePlan::preview()` returns a `Preview` with `notes: Vec::new()` always empty. **Scope note:** the 1-disk warning and the keyfile-asymmetry `WARNING:` block stay inside the interactive confirmation path (`!yes` only) exactly as today -- they are not `PreviewNote`s, do not appear in dry-run, and do not appear on `--yes` real-runs. `execute()` owns the confirmation block, passphrase read/verify, `check_new_not_in_pool`, inhibitor acquisition, journal write, and the existing mutation sequence.

## Migration order

Pattern-setter first, then increasing surface area. Each step either adds one new `PreviewNote` shape or reuses earlier work -- no step introduces a novel pattern that earlier steps didn't vet.

1. **PR 0**: land `preview.rs` with types + `Preview::render` + serde derives + `ProbeEvent::to_preview_note` + shared helpers (`render_per_disk_notes` with `PerDiskStyle::{Bracketed, Plain}`). Zero callers yet. Unit tests for the renderer (pin byte format vs `mount::render_probe_events` and `Step::render_dry_run`). Command-local helpers (`format_add_keyfile_asymmetry_warning`, `format_recover_entry`, `format_add_noop`) land in their respective command files alongside the migration PR that first uses them, not here.
2. **PR 1**: `lock` (pattern baseline, no new `PreviewNote` shapes exercised).
3. **PR 2**: `enroll` (introduces `PreviewNote::PerDisk { level: Skip }` *and* the first Shape A "notes survive Err" implementation -- its mid-loop `probe_config_disk(...)?` is a smaller blast radius than `plan_open_pool`, so it's a good place to nail the pattern before `unlock`/`recover` adopt it).
4. **PR 3**: `remove-missing` (introduces `PreviewNote::Warn` via ENOSPC).
5. **PR 4**: `remove` (same shape as `remove-missing`, adds confirmation-moves-to-execute).
6. **PR 5**: `unlock` (first `ProbeEvent::to_preview_note` user end-to-end).
7. **PR 6**: `recover` (reuses unlock's adapter, adds `PreviewNote::Info` entry-banner pattern).
8. **PR 7**: `add` (two pre-plan warnings, keyfile helper).
9. **PR 8**: `replace` (Shape B with zero-note preview; confirmation-only warnings stay out of `Preview`).

Each PR is independently landable. Commands keep using `Step::print_dry_run` until their turn; no dual-output-path flag.

## Shared helpers

- **`ProbeEvent::to_preview_note(&self) -> PreviewNote`** in `mount.rs`. Per-variant `PerDisk { name, level: Ok|Skip, message: "found"|"not found (unplugged?)"|... }`. `AlreadyMounted` -> `Info("pool already mounted at <mp>")`. `mount::render_probe_events` then becomes a thin wrapper that converts events to notes and pipes through `render_per_disk_notes(..., PerDiskStyle::Bracketed)` -- same bytes, one source of truth for wording.
- **`render_per_disk_notes(notes: &[PreviewNote], style: PerDiskStyle) -> String`** in `preview.rs`, where `enum PerDiskStyle { Bracketed, Plain }`. `Bracketed` preserves `mount`'s `[ok  ]  disk: <name>    <message>` / `{:<10}` format (keeps `mount.rs:1868` byte-for-byte). `Plain` emits `<tag>: <name> <message>` (enroll's current unbracketed shape). `Preview::render` always uses `Bracketed` -- dry-run output is canonical regardless of source command. Real-run paths pick the style that preserves their existing stderr wording. **Scope call-out:** enroll's real-run stderr skip lines today are `skip: diskX not present` (plain); the migration keeps them that way by passing `PerDiskStyle::Plain` in `cmd_enroll_key_file`. This means dry-run preview wording (`[skip]  disk: diskX    not present`) differs from real-run stderr wording for enroll -- accepted as a consequence of "two products, two formats". Unlock and recover keep their existing bracketed per-disk wording; add/remove/remove-missing normalize plan-derived warnings to canonical `[warn]  ...`; replace keeps its confirmation-only warnings outside this helper.
- **`format_recover_entry(journal: &Journal) -> String`** in `recover.rs`. Consumed by both modes.
- **`format_add_keyfile_asymmetry_warning() -> String`** in `add.rs`. **Scoped to `add` only** in this migration. Returns the warning body (no legacy `WARNING:` prefix); `plan_add` wraps the result in `PreviewNote::Warn`; all plan-derived render paths then use the canonical `[warn]  <body>` format. `replace`'s analogous keyfile warning is intentionally NOT unified here -- see the `replace` recipe for why.
- **`format_add_noop(label: &str) -> String`** in `add.rs`. Shared by dry-run Info note and real-run `eprintln!`.

## Test strategy

**Contract layer: CLI subprocess tests.** Stdout/stderr routing is a wire contract; unit tests on `preview.render()` cannot enforce it. Pattern mirrors `tests/cli/braid-unlock.py:125-130`:

```python
machine.succeed(f"{cmd} --dry-run >/tmp/out 2>/tmp/err")
out = machine.succeed("cat /tmp/out")
err = machine.succeed("cat /tmp/err")
assert "<command-specific marker>" in out
assert err.strip() == "", f"unexpected stderr on successful dry-run: {err!r}"
```

Each migrated command gets **four** subtest categories in its existing VM test (or a new one if none):

- **Stepful success**: `--dry-run` with real work to plan. Returns 0, step lines on stdout, stderr empty. Required cases include:
  - Each command's canonical "happy path" preview (already covered by existing tests; migrate their assertion shape to the new renderer).
  - `recover --dry-run` on an already-mounted pool with a pending journal: asserts entry-banner `Info` + `AlreadyMounted` `Info` both appear on stdout, followed by the state-recovery steps (`write recovered pool.json`, `clear pending-op.json`), stderr empty. Pins the note-order-then-steps contract for `recover`.
- **Note-only success** (true zero-step previews only): `--dry-run` that produces `notes` but **zero `steps`**. Returns 0, notes on stdout, stderr empty, **and no step lines present**. This is the guardrail against silent regression of the new "notes with zero steps" contract. Required cases:
  - `add --dry-run` targeting a disk name already in the pool (exercises `PreviewNote::Info("nothing to do -- <disk> already in pool")` + zero steps).
  - `unlock --dry-run` when the pool is already mounted (exercises `ProbeEvent::AlreadyMounted` -> `Info` note + zero steps).
  - `lock --dry-run` on an unmounted pool with no open mappers (existing `nothing to do.\n` contract; already covered by `lock.rs:1168`, keep as guardrail).
  - `recover`'s already-mounted case is **not** a note-only success -- it emits state-recovery steps (`write recovered pool.json`, `clear pending-op.json`). Its coverage lives under stepful success below.
- **Stream-routing regression tests -- one per diagnostic branch that moved from stderr to `PreviewNote`**. Each asserts the diagnostic now appears on stdout *and* stderr stays empty on the success path. Assertions match the canonical `[warn]  <body>` / `[skip]  disk: <name>  <body>` form (legacy `warning:` / `WARNING:` prefixes are stripped at note construction per the render contract):
  - `enroll --dry-run` with at least one absent disk: assert stdout contains `[skip]  disk: <absent>    not present` (bracketed, `{:<10}`-padded), stderr empty. (Moved from `enroll_key_file.rs:55-83`.)
  - `enroll --dry-run` with at least one present-but-not-LUKS disk: assert stdout contains `[skip]  disk: <name>    not LUKS-formatted`, stderr empty. (Same source.)
  - `remove --dry-run` in an ENOSPC-soft-warn scenario (check fails but we proceed): assert stdout contains `[warn]  ENOSPC pre-flight check failed` (body only, no legacy `warning:` prefix), stderr empty. (Moved from `remove.rs:255/270`.)
  - `remove-missing --dry-run` in an ENOSPC-soft-warn scenario: same canonical form, from `remove_missing.rs:260/268`.
  - `add --dry-run` with existing pool drives carrying an `enroll` keyfile but no `--enroll` on the new disk: assert stdout contains `[warn]  Existing pool drives have a keyfile (keyslot-1)` (body only, no `WARNING:` prefix), stderr empty. (Moved from `add.rs:365-371`.)
  - `add --dry-run` with a pool that has missing devices: assert stdout contains `[warn]  pool has N missing device(s)` (body only, no `warning:` prefix), stderr empty. (Moved from `add.rs:348-354`.)
  - `replace`: add a dedicated preview/warning VM test that pins the zero-note preview contract on both live and missing paths, and asserts the confirmation-only `WARNING:` lines do not leak into dry-run or `--yes`.
- **Failure with preserved context** (Shape A commands only): trigger a preview-generation failure *after* probing has accumulated notes. Assert stdout empty, stderr contains the accumulated notes *followed by* the error message, exit nonzero. This pins the new Shape A contract: accumulated notes survive the `Err` path and render on stderr, matching today's `print_probe_events` + `?` behavior. Required cases:
  - `unlock --dry-run` on a degraded pool without `--allow-degraded`: stderr shows per-disk probe lines (bracketed) then the `DegradedRefused` error.
  - `recover --dry-run` when `plan_open_pool` refuses after partial probing: same shape.
  - `enroll --dry-run` with all disks absent: the discovery loop accumulates `skip: <name> not present` notes for every member, then returns `Err("no present LUKS disks found to enroll keyfile into")` at `enroll_key_file.rs:76-79`. Assert stderr contains each `skip: ...` line (plain) followed by the validation error message; exit 1. This pins the preserved-context contract for `enroll`'s no-candidates path -- today's loop already prints skips before the error propagates; the migration must not regress that ordering.
  - `enroll --dry-run` with a mid-loop `probe_config_disk(...)?` failure after partial progress (if a fixture is feasible): assert accumulated skips precede the probe error on stderr. Optional -- only add if a reliable fixture exists; the all-absent case is the primary preserved-context guardrail.
  - `add`: add one preserved-context case where a warning is accumulated before a later planner failure.
  - `remove` / `remove-missing`: no preserved-context case required -- both planners intentionally keep `report.notes` empty and only surface notes on successful plans.
  - `replace`: no preserved-context case required -- Shape B, no notes-carrying report.
- **Failure without context** (all commands): trigger a failure before any note accumulation (missing pool.json, nonexistent device spec, etc.). Assert stdout empty, stderr contains only the error, exit nonzero.
- **Real-run wording preservation / no-widening** -- one subtest per migrated diagnostic whose real-run contract matters. These complement the dry-run stream-routing tests above by pinning the real-run channel+wording. Required cases:
  - `add` (no `--dry-run`) with missing devices: assert stderr contains the **exact** canonical line `[warn]  pool has 1 missing device. Consider repairing with \`braid replace --missing-id <devid>\` first. Use \`braid status\` to see device IDs.`. Singular/plural branch (device vs devices) picked per fixture.
  - `add` (no `--dry-run`) with keyfile asymmetry: assert stderr contains the **exact full canonical block** emitted by `preview::render_notes_for_stderr` over the body from `format_add_keyfile_asymmetry_warning()`:
    ```
    [warn]  Existing pool drives have a keyfile (keyslot-1) for auto-unlock, but the new drive will not.
      Passphrase unlock still works, but the keyfile won't unlock the new drive until it's enrolled.
      Fix: re-run with --enroll <dir>, or run `braid enroll <dir>` afterward.
    ```
    Full-block match (not substring), including trailing `\n` and the 2-space-indented continuation lines.
  - `remove` (no `--dry-run`) in ENOSPC-soft scenario: assert stderr contains the exact canonical `[warn]  ENOSPC pre-flight check failed: ...; proceeding anyway` line.
  - `remove-missing` (no `--dry-run`) in ENOSPC-soft scenario: same exact-match requirement for the canonical `[warn]  ENOSPC pre-flight check failed: ...; proceeding anyway` line.
  - `replace --yes` in the keyfile-asymmetry condition: assert stderr does **not** contain any of the three `WARNING:` / `Passphrase unlock` / `Fix: re-run with --enroll` lines. Pins today's `!yes`-gated behavior against silent widening.
  - `replace` (no `--yes`) with the keyfile-asymmetry condition: assert stderr contains the **exact full three-line block** from `replace.rs:183-189`. Full-block match. This provides a canary that the confirmation-only warning stays out of `Preview` and out of `--yes`.
  - `replace` (no `--yes`) with a 1-disk-post-replace condition: assert stderr contains the exact single-line `WARNING: This replace leaves only 1 disk -- no redundancy.` from `replace.rs:177`. Full match.
  - `enroll` (no `--dry-run`) with at least one absent disk: assert stderr contains `skip: <disk> not present` (plain, pre-passphrase); `enroll_key_file.rs:65` wording unchanged.
  - `enroll` (no `--dry-run`) with at least one present-but-not-LUKS disk: assert stderr contains `skip: <disk> not LUKS-formatted` (plain, pre-passphrase); `enroll_key_file.rs:68` wording unchanged.
  - `enroll` (no `--dry-run`) with at least one disk whose keyfile is already enrolled: assert stderr contains the exact string `ok: <disk> -- keyfile already enrolled` (with `--`, post-passphrase); `enroll_key_file.rs:155` wording unchanged. (Ordering is pinned by the separate wrong-passphrase regression test; no prompt-completion boundary to assert since the existing VM fixtures use `--passphrase-stdin`.)
  - `enroll` (no `--dry-run`) with at least one candidate needing enrollment: assert stderr contains the exact string `enroll: <disk> -- will add keyfile to slot 1` (with `--`, post-passphrase); `enroll_key_file.rs:168` wording unchanged.
  - `enroll` (no `--dry-run`) with a wrong passphrase: assert stderr does NOT contain any `ok: ...` or `enroll: ...` status line before the error message -- pins that the post-passphrase status lines only appear on a successful `plan_enrollment`. (`verify_enrollment_passphrase` / verify in plan_enrollment bails before `:155` / `:168`.)
  - `unlock` / `recover` (no `--dry-run`) stderr probe-event wording: already pinned by `braid-unlock.py:115-153` for the already-mounted case; extend with a degraded-refused real-run subtest to assert per-disk lines + refusal message appear on stderr before the error. Pins `mount.rs:1868`'s wording in the real-run path.

| Command | Target file | Failure trigger |
|---|---|---|
| `lock` | `tests/cli/braid-lock.py` | missing `pool.json` -> exit 1 |
| `unlock` | `tests/cli/braid-unlock.py` (extend) | 3-disk pool, 1 absent, no `--allow-degraded` -> exit 1 |
| `recover` | `tests/cli/braid-recover.py` | no pending journal -> exit 1 |
| `add` | `tests/cli/braid-add-disk.py` | disk spec pointing at nonexistent `/dev/disk/by-id/` path -> exit 1 |
| `enroll` | `tests/cli/braid-enroll.py` + `braid-enroll-generate.py` | keyfile path points at non-regular file (exists, not a file) -> exit 1. **All-disks-absent is a preserved-context failure, not no-context**; see the Shape A preserved-context bucket below. |
| `remove` | `tests/cli/braid-remove-disk.py` | disk name not in pool -> exit 1 |
| `remove-missing` | `tests/cli/braid-remove-missing-enospc.py` | hard ENOSPC -> exit 1 |
| `replace` | `tests/cli/replace-live-disk.py` + `replace-dead-disk.py` | `--old` typo -> exit 1 |

Register any new VM test file in `flake.nix` (current registrations at lines ~365-366 and ~491-492). **Failing to register is a silent skip** -- see memory `feedback_new_vm_test_must_register_in_flake.md`.

**Rendering layer: Rust unit tests.** Existing `dry_run_render_*` tests currently call `Step::render_dry_run(&steps)`; migrate each to `plan_xxx(...).unwrap().preview().render()`. They continue to pin preview content. Specific ones to update:

- `lock.rs:1072,1123,1168,1797`
- `unlock.rs:766,842`
- `add.rs:2216,2272`
- `enroll_key_file.rs:1476,1505`
- `remove.rs:880,932`
- `remove_missing.rs:1214`
- `mount.rs:1868` (`render_probe_events_formats_mixed_probe_result` -- keep the test, re-route through `preview::render_per_disk_notes`; bytes must be identical).

New unit tests worth adding:

- `preview.rs`: render ordering (notes -> steps -> nothing-to-do fallback -> Partial footer).
- `add.rs`: missing-devices count populates a `PreviewNote::Warn` in `AddPlan::preview()`.
- `remove-missing.rs`: ENOSPC-soft path surfaces a `PreviewNote::Warn`; ENOSPC-hard path returns `Err`.
- `recover.rs`: `PreviewNote::Info` from `format_recover_entry` is the first note.

## Critical files (re-read before each PR)

- `/Users/dan/Code/braid/cli/src/lock.rs` -- `render_lock_dry_run:185-217` (target shape) and real-run orphan warn `:258-264`.
- `/Users/dan/Code/braid/cli/src/mount.rs` -- `ProbeEvent:144-152`, `PlanReport:156-159`, `plan_open_pool:171-190`, `render_probe_events:314-355`, `print_probe_events:360-365`.
- `/Users/dan/Code/braid/cli/src/recover.rs` -- entry `eprintln!:158-162`, dry-run branch `:168-227`, already-mounted reconciliation `:187-207`, real-run `print_probe_events:240`.
- `/Users/dan/Code/braid/cli/src/cmd.rs` -- `Step:253-281` (byte format `[{:<11}] {description}`).
- `/Users/dan/Code/braid/cli/src/enroll_key_file.rs` -- `discover_enrollment_candidates:55-83` refactor target.
- `/Users/dan/Code/braid/tests/cli/braid-unlock.py` -- `:100-153` is the exemplar stdout/stderr subprocess test shape.
- `/Users/dan/Code/braid/flake.nix` -- VM test registration block around `:365-366` and `:491-492`.

## Verification

Per PR:

1. `just test-rust` -- new/updated unit tests pass.
2. `just test-vm <cmd>` -- the VM test(s) for the migrated command pass.
3. Manual spot-check of one migrated command: `braid <cmd> --dry-run >/tmp/o 2>/tmp/e; wc -c /tmp/e` should be 0.
4. Grep `cli/src/` for `eprintln!` inside the migrated command's module -- every surviving call must be inside `execute` or a clearly real-run-only path. Any `eprintln!` reachable from a dry-run branch is a regression.

The per-command VM subprocess tests (in the matrix above) are the stdout/stderr contract enforcement layer. Do not add a crate-wide `cargo`-based guard -- these commands are root-gated and stateful (need real LUKS/btrfs), so a cargo integration test would either fail to exercise them or duplicate the VM tests behind a second fake runtime.

## Risks / open questions

- **Real-run `enroll` skip lines**: today `discover_enrollment_candidates` prints `skip: X not present` on every invocation, real-run included. Migration preserves this by having `cmd_enroll_key_file` call `preview::render_notes_for_stderr(&plan.notes, PerDiskStyle::Plain)` and `eprint!` the result before credential prompting. Dry-run preview renders the same notes in `Bracketed` style on stdout -- wording diverges between modes (`skip: X not present` real-run vs `[skip]  disk: X  not present` dry-run), documented scope-note. Test: extend `braid-enroll.py` real-run subtest to assert the legacy `skip: ...` (plain) wording still appears on stderr.
- **`unlock` Test 2c "already-mounted" stderr**: `braid-unlock.py:115-153` pins `pool already mounted at <mp>` to stderr for real-run. The migration preserves this via `plan.execute`'s top-of-body `eprint!(render_notes_for_stderr(&self.notes, Bracketed))` -- since `plan.notes` contains the converted `AlreadyMounted` -> `Info` note, execute's stderr output is byte-identical to today's `print_probe_events` call for this case. No raw `probe_events` field is retained on the plan.
- **`recover` already-mounted reconciliation** (`:186-207`) runs only in dry-run today. The migration preserves that asymmetry; real-run reconciliation happens implicitly downstream. Do not silently widen to both modes -- a deliberate behavior change deserves its own PR.
- **`PreviewGap` empty enum**: serde-tagged empty enums are fine, but the first variant must adopt `#[serde(rename_all = "kebab-case")]` discipline to keep JSON keys stable. Call that out in PR 0's code review.
- **`ProbeEvent::to_preview_note` wording**: must match byte-for-byte the current `render_probe_events` output, otherwise `render_probe_events_formats_mixed_probe_result` (`mount.rs:1868`) fails. Explicit test: a new `mount.rs` test `probe_event_to_preview_note_preserves_byte_format` that renders each variant through both paths and asserts string equality.
- **No other commands carry real-run banners like `recover`'s**: confirmed by greping `eprintln!` in the 8 files -- only `recover:158-162` qualifies. If a future command adds one, the `format_*_entry` pattern generalizes.

## Out of scope

- `--format json` output. `Preview` types get `#[derive(Serialize)]` so a follow-up can wire `--format` cheaply, but no flag ships this round.
- A shared `CommandPlan` trait. Convention is sufficient today; revisit with JSON.
- Migrating non-dry-run commands to the plan-object layering. The eight listed commands are the scope.
