# Plan: unify UPS severity classification and surface it on `braid ups status`

## Context

`braid ups status`'s human render is the only one of three UPS surfaces that
emits raw NUT tokens with **no severity interpretation**. The TUI color-codes
the same flags (`tui::view::ups_severity_color`), and `--json` carries a
documented affirmative-`OL` criterion plus a `ups_status_empty` warning. A
human running `braid ups status` during an outage sees `Status: OB LB` with no
cue that this is the critical pair preflight refuses on -- it leans on the
operator already knowing NUT vocabulary, the exact friction braid exists to
remove ("run a NAS without fiddling with manpages", AGENTS.md).

This also exposes a structural gap: the four-way severity ladder
(critical / on-battery / online / indeterminate) is currently derived in **two
places** -- `ups_severity_color` (raw `contains` checks) and
`preflight::check_ups_not_on_battery` (three sequential predicate calls). The
codebase deliberately shares `UpsStatusFlag::is_critical` between preflight and
the TUI "so the two surfaces never disagree"; this plan finishes that job.

**Outcome:** one `UpsSeverity` classifier is the single source of truth; the
TUI color, the `braid ups status` human cue, and the mutation safety gate all
consume it. The human render gains a colored severity tag + one-word plain
gloss, reusing the existing `status_tag` house style, while preserving the raw
NUT tokens.

## Approach

### 1. New domain enum + classifier (`cli/src/parse/types.rs`)

Add next to `UpsStatusFlag` / the `UpscOutput` classifier block (sibling
precedent: the existing `SmartHealth` status enum):

```rust
pub enum UpsSeverity { Online, OnBattery, Critical, Indeterminate }
```

- `UpsSeverity::classify(flags: &[UpsStatusFlag]) -> UpsSeverity` -- operates on
  a flag slice (the TUI holds `Vec<UpsStatusFlag>`, not a full `UpscOutput`, so
  the classifier must NOT require `UpscOutput`). Ladder, in exactly today's
  `ups_severity_color` order: any `is_critical` flag -> `Critical`; else `Ob`
  present -> `OnBattery`; else `Ol` present -> `Online`; else (incl. empty set,
  unknown-only tokens) -> `Indeterminate`.
- `UpscOutput::severity(&self) -> UpsSeverity` -- delegates to
  `classify(&self.status_flags)`.
- Doc comment: this is the single severity authority shared by preflight, the
  TUI, and the CLI human render; classification lives here only. Classification
  is pure domain -- no presentation (Color / StatusTag / English) lives in
  types.rs.

Keep `is_critical` / `is_on_battery` / `reports_utility_power` (still used /
referenced); `classify` is built from the same predicates, so they remain the
primitives.

### 2. TUI consumes the classifier (`cli/src/tui/view/mod.rs`)

Refactor `ups_severity_color` to `match UpsSeverity::classify(flags)`:
`Critical => Red`, `OnBattery => Yellow`, `Online => Green`,
`Indeterminate => DarkGray`. Behavior is unchanged, so the existing
`ups_severity_*` unit tests stay green and become the regression guard on the
mapping. `format_ups_flags` is untouched.

### 3. Human cue (`cli/src/ups.rs`)

- Change `format_human(name, parsed)` ->
  `format_human(name, parsed, color_enabled: bool)`, mirroring doctor's
  `format_doctor_human_with(report, color_enabled)`. Caller `cmd_ups_status`
  passes `status_tag::color_enabled_for_stdout()`.
- Non-empty status: render `Status: {tokens}  {tag} {gloss}` -- exactly **two
  spaces** between tokens and tag, **one space** between tag and gloss, no
  column alignment (token width varies, so there is nothing to align to).
  `{tag}` = `status_tag::render_status_tag(tag, color_enabled)` (canonical
  colored/plain `[ok]/[warn]/[fail]/[skip]` bytes, already TTY+`NO_COLOR`
  gated); `{tokens}` via the unchanged `format_status`. Canonical plain-form
  lines the tests pin:
  - `Status: OL  [ok] on utility power`
  - `Status: OB  [warn] on battery`
  - `Status: OB LB  [fail] critical`
  - `Status: WEIRD  [skip] utility power not confirmed`
- Empty status: leave `Status: (unknown -- ups.status missing)` **exactly as
  today** -- no tag. That sentinel is load-bearing for the doctor/preflight
  referral and pinned by `format_human_empty_status_renders_sentinel`; it
  already self-describes, so no cue is added.
- Private map in `ups.rs` (presentation lives at the surface, not in types.rs):

  | `UpsSeverity`   | StatusTag | gloss                         |
  | --------------- | --------- | ----------------------------- |
  | `Online`        | `Ok`      | `on utility power`            |
  | `OnBattery`     | `Warn`    | `on battery`                  |
  | `Critical`      | `Fail`    | `critical`                    |
  | `Indeterminate` | `Skip`    | `utility power not confirmed` |

  `Critical` gloss is intentionally generic: the bucket spans
  LB / TESTFAIL / COMMBAD / FSD, and `OL TESTFAIL` / `OL COMMBAD` are critical
  while still on utility power -- so "shutdown imminent" would be false. The
  preserved NUT token supplies specifics; braid adds only the severity word
  (consistent with the `format_status_ol` keep-NUT-vocabulary rationale). The
  gloss describes the power **condition** only and must not claim mutation
  readiness (the guide makes the `add`/`remove` refusal error, not `status`,
  the readiness oracle).

### 4. Safety gate consumes the classifier (`cli/src/preflight.rs`)

In `check_ups_not_on_battery`, keep the query-failed / invocation-failed /
empty-status pre-checks unchanged, then replace the three sequential
`is_critical` / `is_on_battery` / `!reports_utility_power` ifs with one
exhaustive match:

```rust
match parsed.severity() {
    UpsSeverity::Online        => Ok(()),
    UpsSeverity::Critical      => refuse("UPS reports a critical state (LB / TESTFAIL / COMMBAD / FSD)"),
    UpsSeverity::OnBattery     => refuse("UPS reports on-battery"),
    UpsSeverity::Indeterminate => refuse("UPS does not report utility power (OL missing)"),
}
```

Refusal strings stay **byte-identical** to today. The mapping is
behavior-preserving (critical>OB>OL>else == current branch order). The
exhaustive match is a fail-closed win: a future 5th severity variant becomes a
compile error in the safety gate until someone decides how it is treated.

## Reuse (do not reinvent)

- `cli/src/status_tag.rs`: `render_status_tag`, `StatusTag`,
  `color_enabled_for_stdout`, and the `testing::capture_with_color` /
  color-override seam. Public, used by doctor + ~13 commands, endorsed by
  `docs/design/principles.md`.
- `cli/src/doctor.rs#format_doctor_human_with` is the precedent for threading
  `color_enabled: bool` into a formatter and mapping a domain status enum
  (`CheckStatus`) to `StatusTag`.
- `cli/src/parse/types.rs#UpsStatusFlag::is_critical` and the `UpscOutput`
  predicates remain the classification primitives.

## Tests

- **types.rs** (next to `ups_status_flag_critical_set`): `UpsSeverity::classify`
  unit tests -- critical-wins-over-`OL` (`OL TESTFAIL` -> Critical),
  **`OL OB` -> OnBattery (pins OB winning over OL -- the ladder must check OB
  before OL, else a contradictory pair would render/pass as online)**, `OB` ->
  OnBattery, `OL` -> Online, `OL RB` -> Online (advisory), empty -> Indeterminate,
  unknown-only token (`WEIRD`) -> Indeterminate. Behavioral, structure-insensitive.
- **preflight.rs**: a full `check_ups_not_on_battery` test block already exists
  (`ups_no_config_is_noop` .. `ups_online_with_unknown_token_passes`, the
  `// --- check_ups_not_on_battery tests ---` section, ~`preflight.rs:1940`),
  driven by the local `upsc_mock` helper. This work **updates** it -- it does
  not start from a blank slate. The exhaustive-match refactor is
  behavior-preserving, so these mostly stay green; the change is to **tighten
  loose, context-insensitive assertions** so they pin the per-severity message
  the refactor now guarantees:
  - `ups_low_battery_refuses` (`OL LB`): replace
    `err.contains("critical") || err.contains("on-battery")` with a tight
    `err.contains("critical")` **and** `!err.contains("on-battery")` -- `OL LB`
    is Critical (LB), not OnBattery; the `||` currently lets the wrong branch
    pass.
  - `ups_on_battery_refuses` (`OB`): tighten from the catch-all
    `err.contains("utility power")` (true of every refusal) to
    `err.contains("on-battery")`.
  - `ups_empty_status_refuses`: tighten to assert the empty-context substring
    (e.g. `"empty or missing"`) rather than the catch-all `"utility power"`.
  - **Add** `ups_on_battery_with_ol_refuses` (`ups.status: OL OB`) asserting
    `err.contains("on-battery")` -- pins OB winning over OL at the gate (the
    existing OB test uses a bare `OB`, so the contradictory pair is unpinned).
  - Leave the already-tight tests as-is (`ups_test_fail_refuses`,
    `ups_comm_bad_refuses`, `ups_fsd_refuses` assert `"critical"`;
    `ups_status_without_ol_refuses` asserts `"OL missing"`; the query/invocation
    cases). All assertions stay substring-on-context (structure-insensitive).
- **ups.rs**: `cargo insta` re-accept the 4 `snapshot_human_*` snapshots
  (online / onbattery / lowbattery / replace-battery) under
  `cli/src/snapshots/` -- snapshots render with `color_enabled = false` (plain
  tags) for stable bytes. Note the 4 fixtures are all `OL`/`OB`, so none
  exercises the Indeterminate arm; add an explicit `format_human` unit test for
  a **non-empty indeterminate** status (`ups.status: WEIRD`, `color_enabled =
  false`) asserting the exact line `Status: WEIRD  [skip] utility power not
  confirmed` -- this is the only guard that the `[skip]` tag + gloss are emitted
  and mapped correctly. Add one `format_human` test with `color_enabled = true`
  pinning the ANSI-wrapped tag on a critical render (mirrors
  `status_tag_pins_colored_levels`). `format_status_*` and the empty-sentinel
  test stay green (those paths untouched).
- **tui/view**: existing `ups_severity_*` tests unchanged -- they guard the
  refactor.

## Docs

- `docs/commands/ups-status.md` (Basic example block) and `docs/guides/ups.md`
  (Example human output block): update the rendered example to show the tag +
  gloss on the `Status:` line; add one sentence that the human `Status:` line
  carries a severity tag (`[ok]`/`[warn]`/`[fail]`/`[skip]`, colored on a TTY,
  plain under `NO_COLOR`/pipes) while preserving raw NUT tokens. The guide's
  "TUI UPS panel" color list already documents the ladder -- cross-reference it
  as the shared severity model rather than restating.
- Reconcile the salvage item: the `snapshot_human_onbattery` /
  `snapshot_human_lowbattery` test comments already claim the render lets
  operators understand severity "at a glance" -- that intent now actually holds;
  leave the comments (or tighten to reference the tag).

## Invariants preserved

- **ADR 020 token order:** the cue reads flags as a set; `format_status` still
  renders the `Vec` verbatim with no reorder.
- **ADR 020 mutation gate:** refusal strings byte-identical; exhaustive match is
  fail-closed; the existing `check_ups_not_on_battery` test block is tightened
  (loose `||` / catch-all asserts -> exact per-severity context) and extended by
  one contradictory-pair (`OL OB`) test.
- **Empty sentinel** `(unknown -- ups.status missing)` is untouched.
- **ASCII rule:** tags + gloss are ASCII; ANSI color escapes are the sanctioned
  exception (`status_tag`, principles.md).

## Verification (end-to-end)

1. `just test-rust` -- unit tests + insta; `cargo insta review` to accept the 4
   human snapshots.
2. `cargo build` the CLI; run against the dummy-ups path used by the VM tests:
   - `braid ups status` on a TTY shows the colored tag + gloss;
   - `braid ups status | cat` and `NO_COLOR=1 braid ups status` stay plain
     `[fail]` etc.;
   - exercise online / on-battery / low-battery / empty-status fixtures and eyeball
     each `Status:` line.
3. `scripts/docs/check-output-ascii.py` over `cli/src/**/*.rs` (gloss/tags are
   ASCII).
4. `just docs-build` -- mdbook linkcheck for the edited pages.
5. If any NixOS VM test in `tests/` asserts `braid ups status` human output,
   update its expectation to include the tag (plain form).
