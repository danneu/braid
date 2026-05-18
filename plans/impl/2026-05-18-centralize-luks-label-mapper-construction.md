# Plan: centralize disk-name-derived label and mapper construction

## Context

A code-review finding flagged that `format!("braid-{}", new_name)` is duplicated in the FreshLuks replace path. Verification surfaced that the duplication is wider than the finding stated: 12 production LUKS-label sites in `add.rs`, `replace.rs`, and `recover.rs` re-derive `format!("braid-{name}")` ad hoc, plus the sibling mapper-name helper at `cli/src/config.rs:71-73` already exists but takes `&str` so it does not enforce the `DiskName` validation contract at construction time.

The user explicitly chose the maximal-ideal scope: type-discipline at every label/mapper boundary, no asymmetry, no surface area for unvalidated strings to become mapper or label bytes.

Endpoints:

- A new `LuksLabel(String)` newtype paralleling `MapperName`, with a **private inner field** and a single constructor `LuksLabel::for_disk(&DiskName)` so the "only producer is the helper" invariant is enforced by the type system, not by convention.
- A new helper `pub fn luks_label_for(name: &DiskName) -> LuksLabel` that is a thin wrapper around `LuksLabel::for_disk`. Callers may use either form; both produce the same value, and there is no other way to construct a `LuksLabel`.
- The existing `pub fn mapper_name(name: &str)` becomes `pub fn mapper_name(name: &DiskName) -> MapperName`, so the helper enforces the validated type the same way as `luks_label_for`. `MapperName`'s inner field stays `pub String` (pre-existing convention; many call sites access `.0`) -- tightening `MapperName` is a separate cleanup outside this plan.
- Internal data structures that currently carry a disk identity as `String` (`DiskEnrollAction::name`, `OpenPlan::to_unlock`, `mount.rs::first_open_mapper`, `EnrollmentCandidate`) move to `DiskName` so the helpers can be called without re-parsing.
- The 12 inline label sites, ~38 mapper-name call sites, and the related function signatures all converge on the two helpers.

Out of scope: NixOS module / Python VM tests (they hardcode `braid-disk1` etc. in tool-output assertions; not part of the Rust invariant).

No backwards compatibility / migration shims (per project policy).

## Design decisions (locked)

| Decision | Choice | Rationale |
|---|---|---|
| `luks_label_for` signature | `(name: &DiskName) -> LuksLabel` | Validated-type-at-boundary; same shape as `mapper_name` after its change. |
| `mapper_name` signature | Change to `(name: &DiskName) -> MapperName` | Symmetric type discipline with the new label helper. |
| `LuksLabel` newtype | Add `pub struct LuksLabel(String)` with private inner field; sole constructor is `LuksLabel::for_disk(&DiskName)`. | Privacy makes the "only producer is the helper" invariant true by construction. A `pub` inner field would allow any caller to synthesize a `LuksLabel("anything".into())`, which the plan's own claim forbids. |
| Helper location | `cli/src/config.rs` (helpers) + `cli/src/types.rs` (newtype) | Match existing layout: `MapperName` lives in `types.rs`; `mapper_name` factory lives in `config.rs`. |
| Doc-comment updates | Update `journal.rs:41,73,139` and `types.rs:100-103` (`DiskName`) to reference helper names. | Comments otherwise go stale. |
| Embedded message at `recover.rs:866` | Bind `let expected_label = luks_label_for(&target.name);` and inject inside the literal sentence quotes. | Byte-identical user output; the inline `format!` disappears. |
| Order of operations | Single atomic commit. | The signature changes and the data-structure type changes can't be split without intermediate states that don't compile. |

## Critical files

- `cli/src/types.rs` -- add `LuksLabel`; update `DiskName` doc comment.
- `cli/src/config.rs` -- add `luks_label_for`; change `mapper_name` signature; update + add unit tests.
- `cli/src/cmd.rs` -- `CmdRequest::CryptsetupLuksFormat { label: LuksLabel }` + handler update (`cmd.rs:786`).
- `cli/src/luks.rs` -- `pub fn luks_format(label: &LuksLabel)`; `ensure_luks_open(name: &DiskName)`; `ensure_luks_open_with_key_file(name: &DiskName)`; `classify_mapper_ownership(name: &DiskName)`; in-file test sites.
- `cli/src/add.rs` -- 3 label sites + ~2 mapper sites + `validate_braid_preconditions(name: &DiskName)` + 2 test callers + fixture at `add.rs:7777`.
- `cli/src/replace.rs` -- 2 label sites + several mapper sites + string-literal tests at `2426/2440`.
- `cli/src/recover.rs` -- 6 label sites + ~14 mapper sites.
- `cli/src/mount.rs` -- `OpenPlan::to_unlock: Vec<(DiskName, ByIdPath)>` (struct at `mount.rs:97`) + `first_open_mapper: Option<DiskName>` (`mount.rs:229`) + `mount_key: &DiskName` (`mount.rs:329-333`) + sites at `334, 383, 567`.
- `cli/src/enroll_key_file.rs` -- `pub type EnrollmentCandidate = (DiskName, ByIdPath);` (`enroll_key_file.rs:66`) + `DiskEnrollAction::{AlreadyEnrolled, NeedsEnroll}.name: DiskName` + discovery sites at `90, 133`, slice consumers at `237, 371`, dry-run storage at `430`, sites at `314, 387`.
- `cli/src/probe.rs:216` -- mapper site.
- `cli/src/status.rs:895` -- mapper site.
- `cli/src/tui/probe.rs:35` -- mapper straggler.
- `cli/src/test_fixtures/status.rs:656,670` -- fixtures.
- `cli/src/test_fixtures/remove.rs:258,260` -- fixtures.
- `cli/src/journal.rs:39-43, 72-74, 137-140` -- doc-comment updates (no code).

## Implementation steps

### Step 1 -- `cli/src/types.rs`: add opaque `LuksLabel` newtype with sole constructor

Add immediately after `MapperName` (~line 317). The inner field is private and **no serde derives are added** -- `CmdRequest` itself only derives `Debug, Clone, PartialEq, Eq` (see `cmd.rs:20`), so `LuksLabel` is never serialized or deserialized in this codebase, and adding `Deserialize` would re-introduce the arbitrary-bytes construction path the privacy is meant to prevent:

```rust
/// Wraps a LUKS2 label braid writes into the cryptsetup header so callers
/// cannot accidentally pass an unvalidated string in its place; the inner
/// field is private so the sole constructor is `LuksLabel::for_disk`
/// (re-exported via `config::luks_label_for`). The observed-from-probe
/// label stays `Option<String>` because cryptsetup's reported label may not
/// follow braid's convention.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LuksLabel(String);

impl LuksLabel {
    /// The sole constructor; produces the canonical `braid-<name>` label
    /// from a validated `DiskName`. No other code path can synthesize a
    /// `LuksLabel` from arbitrary bytes.
    pub fn for_disk(name: &DiskName) -> Self {
        LuksLabel(format!("braid-{}", name.as_str()))
    }

    /// Borrow the label text at command argv and probe-comparison
    /// boundaries without exposing the inner string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for LuksLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
```

Privacy notes:

- The field is plain `String`, not `pub String`. Tuple-struct construction (`LuksLabel(s)`) is only callable from inside `cli/src/types.rs`; every other module must go through `LuksLabel::for_disk` or `config::luks_label_for`. There is no `Deserialize` derive, so no serde-driven construction path exists either.
- If a future feature genuinely needs to persist a `LuksLabel` (e.g., journaling a label alongside a record), introduce a *validated* `Deserialize` impl that re-checks the `braid-<DiskName>` shape and parses the suffix back into a `DiskName`. Do not derive `Deserialize` blindly.
- `MapperName(pub String)` stays as-is in this plan: it has ~40 `.0` field accesses across the codebase (display, path construction). Tightening it to the same privacy discipline is a parallel cleanup outside this plan's scope.

Update the `DiskName` doc comment (`types.rs:100-103`) to reference the helper names:

```rust
/// Operator-facing disk identifier used as the mapper-name and LUKS-label
/// suffix (`braid-<DiskName>`). Not a persistent identity -- `LuksUuid` is
/// -- but every label/mapper construction site goes through this type via
/// `config::mapper_name` and `config::luks_label_for` so the disk-name
/// character contract is enforced once.
```

### Step 2 -- `cli/src/config.rs`: add `luks_label_for`, retype `mapper_name`

Replace the current helpers and tests block:

```rust
/// Returns the mapper name for a disk: braid-<name>. Validated-type
/// signature so callers cannot accidentally synthesize a `MapperName` from
/// an unchecked string.
pub fn mapper_name(name: &DiskName) -> MapperName {
    MapperName(format!("braid-{}", name.as_str()))
}

/// Re-export of the sole `LuksLabel` constructor, keeping the
/// `config::luks_label_for(&name)` call-site convention alongside
/// `config::mapper_name(&name)`. The privacy of `LuksLabel`'s inner field
/// (see `types.rs`) means this function is the only public entry point
/// other than `LuksLabel::for_disk` itself.
pub fn luks_label_for(name: &DiskName) -> LuksLabel {
    LuksLabel::for_disk(name)
}
```

(Add the `LuksLabel` import to the module's `use` block.)

Update the `#[cfg(test)] mod tests` block. Both unit tests follow the project's `// Intent / Why it exists / Scenario` preamble convention from AGENTS.md "Test Conventions":

```rust
// Intent: `mapper_name(&DiskName)` returns the canonical `braid-<name>`
//   MapperName for representative disk names.
// Why it exists: pins the convention end-to-end at the helper boundary so
//   a regression that renamed the prefix or dropped the hyphen would be
//   caught here before propagating to argv builders.
// Scenario: planner code holds a validated `DiskName` and asks the helper
//   for the corresponding mapper basename used in `/dev/mapper/<X>` paths
//   and `cryptsetup open --name <X>` argv.
#[test]
fn mapper_name_for_disk() {
    let toshiba = DiskName::parse("toshiba").unwrap();
    let ironwolf = DiskName::parse("ironwolf").unwrap();
    assert_eq!(mapper_name(&toshiba), MapperName("braid-toshiba".into()));
    assert_eq!(mapper_name(&ironwolf), MapperName("braid-ironwolf".into()));
}

// Intent: `luks_label_for(&DiskName)` returns the canonical `braid-<name>`
//   LuksLabel for representative disk names.
// Why it exists: closes the original review finding (12 inline
//   `format!("braid-{name}")` label sites). The unit-level pin means any
//   future drift in the label-prefix convention surfaces here, not in a
//   downstream cryptsetup argv test that only substring-matches.
// Scenario: planner / executor / recovery hold a validated `DiskName` and
//   ask the helper for the expected `braid-<name>` LUKS2 label used in
//   `cryptsetup luksFormat --label <X>` and probe-vs-expected comparisons.
#[test]
fn luks_label_for_disk() {
    let toshiba = DiskName::parse("toshiba").unwrap();
    let ironwolf = DiskName::parse("ironwolf").unwrap();
    assert_eq!(luks_label_for(&toshiba).as_str(), "braid-toshiba");
    assert_eq!(luks_label_for(&ironwolf).as_str(), "braid-ironwolf");
}
```

Note: `luks_label_for(...)` returns a `LuksLabel` whose inner field is private, so the assertions compare via `as_str()` rather than constructing a literal `LuksLabel(...)`. That is intentional -- it exercises the same accessor real callers use, and it removes the only place the plan would otherwise have to expose construction.

### Step 3 -- `cli/src/cmd.rs`: retype `CryptsetupLuksFormat.label` and update every constructor, destructure, and assertion

`cmd.rs:160-165`:

```rust
CryptsetupLuksFormat {
    device: String,
    uuid: LuksUuid,
    label: LuksLabel,
    extra_opts: LuksFormatExtraOpts,
},
```

Update the argv-building handler at `cmd.rs:786`. Read the surrounding code first to make sure the call passes `label.as_str()` to the cryptsetup command builder. No serde involvement; the retype is purely a Rust type change.

Because `CryptsetupLuksFormat` is touched at many constructors, destructuring patterns, and assertion sites across the codebase, this step is **grep-driven**: before declaring done, run `grep -rn 'CryptsetupLuksFormat' cli/src/` and ensure every hit either (a) is a constructor producing the new `label: LuksLabel`, (b) is a destructure that no longer assumes `label: String`, or (c) is a doc-comment / `matches!(_)` skeleton that needs no change. The full inventory at the time of writing:

**Constructors that must produce `label: LuksLabel`** (use `luks_label_for(&name)` or `LuksLabel::for_disk(&name)`; for tests, use `&disk("...")` to get a `DiskName` first):

- `cli/src/cmd.rs:2171, 2203, 2700, 2724` (in-file `#[cfg(test)]`).
- `cli/src/cmd.rs:2924, 3066, 3106` (additional in-file tests covering passphrase-via-stdin discipline and structured-argv invariants).
- `cli/src/luks.rs:2633, 2681` (in-file `#[cfg(test)]`).
- `cli/src/recover.rs:8564` (recover test).
- `cli/src/add.rs:483` (production -- planner emits the request).
- `cli/src/replace.rs:258` (production -- planner emits the request).
- `cli/src/recover.rs:838` (production -- recovery replay emits the request).

**Destructuring patterns that may bind `label` as `&String`** -- change the bound type to `&LuksLabel` and compare with `.as_str()` instead of comparing to a `String` literal:

- `cli/src/add.rs:8126-8133` -- destructures `(uuid, label, extra_opts)` from the recorded `CryptsetupLuksFormat`. The downstream assertion at `cli/src/add.rs:8139` (`assert_eq!(label, "braid-disk2");`) becomes `assert_eq!(label.as_str(), "braid-disk2");`.
- `cli/src/replace.rs:4067, 5945` -- same destructure shape. The assertion at `cli/src/replace.rs:5960` (`assert_eq!(label, "braid-disk3", ...)`) becomes `assert_eq!(label.as_str(), "braid-disk3", ...)`.
- Any other `let CmdRequest::CryptsetupLuksFormat { label, .. } = ...` site that binds `label` and then equates it to a string literal must switch to `label.as_str()`.

**Inspect-only patterns that ignore `label`** -- no change needed. These are `matches!(r, CmdRequest::CryptsetupLuksFormat { .. })` and destructures like `{ device, .. }` that never bind `label`. The grep hits at `cli/src/cmd.rs:1135`, `cli/src/add.rs:4021, 4029, 5288, 5891, 5901, 5982, 6486, 7897, 8243`, `cli/src/replace.rs:3981, 4065, 4137, 5928, 5943`, `cli/src/recover.rs:7648, 8266, 8737, 9036, 9077` fall in this bucket.

The grep-driven discipline matters: a missed constructor produces a compile error (the `label:` field type changed), but a missed assertion that compares a `LuksLabel` against a string literal would produce a less obvious failure mode -- a borrow-mismatch error that points at the assertion line rather than at the missing `.as_str()`. Catching them by inventory is faster than chasing them by error.

### Step 4 -- `cli/src/luks.rs`: retype `luks_format` + `ensure_luks_open*` + `classify_mapper_ownership`

`luks.rs:449`:

```rust
pub fn luks_format<R: CommandRunner>(
    runner: &R,
    device: &str,
    passphrase: &Passphrase,
    uuid: &LuksUuid,
    label: &LuksLabel,
    extra_opts: &LuksFormatExtraOpts,
) -> Result<(), LuksError> {
    let result = runner.run_with_stdin(
        &CmdRequest::CryptsetupLuksFormat {
            device: device.to_owned(),
            uuid: uuid.clone(),
            label: label.clone(),
            extra_opts: extra_opts.clone(),
        },
        ...
```

`luks.rs:919` and `luks.rs:966`: change `name: &str` to `name: &DiskName` on `ensure_luks_open` and `ensure_luks_open_with_key_file`. Inside, `mapper_name(name)` now type-checks.

`luks.rs:836`: change `classify_mapper_ownership(name: &str, ...)` to `(name: &DiskName, ...)`. The `name` parameter is only used in error/log messages downstream, so the change is mostly mechanical. Any `format!("... {name}", name)` works for `&DiskName` (Display delegates to the inner string).

In-file `luks.rs` test sites (`1757, 1807, 1856, 1905, 1954, 2011, 2232, 2289, 2340, ...`) that pass string literals to `ensure_luks_open(&runner, "disk1", ...)` need to construct `DiskName`. The pragmatic move: add a small test helper near the top of the test module:

```rust
fn disk(name: &str) -> DiskName {
    DiskName::parse(name).expect("test disk name")
}
```

Then call sites become `ensure_luks_open(&runner, &disk("disk1"), ...)`. Reuse this helper in `cmd.rs`, `add.rs`, `replace.rs` test modules too (define once per module).

`luks.rs:3194` test fixture: `MapperName(format!("braid-{name}"))` becomes `mapper_name(&disk(name))` (or accept `name: &DiskName` into the fixture helper).

### Step 5 -- `cli/src/mount.rs`: retype `OpenPlan::to_unlock`, `first_open_mapper`, and `mount_key`

`mount.rs:97-99` (`pub struct OpenPlan`): retype the field to `pub to_unlock: Vec<(DiskName, ByIdPath)>`.

`mount.rs:229`: retype `let mut first_open_mapper: Option<DiskName> = None;`. The variable name "mapper" is misleading -- it stores a disk *name*, not a mapper name -- but renaming is a separate concern; leave the name and just retype.

`mount.rs:233`: introduce **two** bindings so display-only sites keep `&str` while only the typed sinks see `&DiskName`. The existing `ProbeEvent::DiskAbsent`, `ProbeEvent::DiskLuksHeaderDamaged`, `ProbeEvent::DiskLuksHeaderUnreadable`, `ProbeEvent::DiskAlreadyOpen`, `ProbeEvent::DiskAvailable` variants all carry `name: String`, and the `missing: Vec<(String, MissingReason)>` collection at `mount.rs:230` also stays `String` (it is a display surface, not a typed name). Replace the line with:

```rust
let disk_name = &member.name;
let display_name = disk_name.as_str();
```

Then in the match body below:

- `mount.rs:243-246`: `events.push(ProbeEvent::DiskAbsent { name: display_name.to_owned() });` and `missing.push((display_name.to_owned(), MissingReason::Unplugged));`.
- `mount.rs:265-273` (the `PresentNotLuks` branch): same `display_name.to_owned()` substitution for the `ProbeEvent::DiskLuksHeaderDamaged`/`DiskLuksHeaderUnreadable` and the `missing.push((display_name.to_owned(), reason))` line.
- `mount.rs:280-289` (the UUID-mismatch `format!`): `name` is read as `Display` -- substitute `display_name` (or `disk_name`, since `DiskName: Display`). Either works; use `display_name` for symmetry with the rest of the block.
- `mount.rs:292-295` (the `DiskAlreadyOpen` event): `events.push(ProbeEvent::DiskAlreadyOpen { name: display_name.to_owned() });`.
- `mount.rs:297-298`: `first_open_mapper = Some(disk_name.clone());` -- this is the only sink that consumes the typed value.
- `mount.rs:301-302` (the `DiskAvailable` event): `events.push(ProbeEvent::DiskAvailable { name: display_name.to_owned() });`.
- `mount.rs:304`: `to_unlock.push((disk_name.clone(), member.by_id.clone()));` -- the other typed sink.

The split keeps the display surfaces (`ProbeEvent.name: String`, `missing.0: String`) untouched while the typed value flows exactly where the later helpers (`mapper_name(mount_key)` etc.) require it. Retyping the `ProbeEvent` and `missing` collections to `DiskName` is a separate, optional cleanup the plan does **not** undertake.

`mount.rs:329-333`: rewrite the `mount_key` computation to produce `&DiskName` directly, dropping the dead `"unknown"` fallback (the pre-condition at line 311 already returns `Err` when both `to_unlock` and `any_open` are empty, so the fallback can never fire):

```rust
let mount_key: &DiskName = to_unlock
    .first()
    .map(|(k, _)| k)
    .or(first_open_mapper.as_ref())
    .expect("post-check above guarantees to_unlock or first_open_mapper is non-empty");
let mount_device = format!("/dev/mapper/{}", mapper_name(mount_key).0);
```

This removes the only `mapper_name(&str)` call that currently relies on a string literal fallback, so the helper's retyped signature compiles without further work.

`mount.rs:383, 567`: `mapper_name(name)` works directly once `name: &DiskName`.

`mount.rs:483, 507, 1144`: `&[(String, ByIdPath)]` parameter and binding signatures become `&[(DiskName, ByIdPath)]`. The `credential_verify_targets` helper at line 483 takes a slice of `(name, by_id)` pairs; trace its callers to ensure the typed value is sourced.

`mount.rs:956`: test fixture build of `to_unlock` -- use the `disk("...")` helper (introduced in Step 4) to construct `DiskName`s.

### Step 6 -- `cli/src/enroll_key_file.rs`: retype `EnrollmentCandidate` and `DiskEnrollAction.name`

The enrollment pipeline carries disk identity as `String` in two places. Both must move to `DiskName` so the helpers below can be called without re-parsing.

`enroll_key_file.rs:66`: retype the alias.

```rust
pub type EnrollmentCandidate = (DiskName, ByIdPath);
```

`enroll_key_file.rs:42-43`: retype both `DiskEnrollAction` variants.

```rust
pub(crate) enum DiskEnrollAction {
    AlreadyEnrolled { name: DiskName, by_id: ByIdPath },
    NeedsEnroll { name: DiskName, by_id: ByIdPath },
}
```

Update discovery (`discover_enrollment_candidates`, ~line 80-148):

- Line 90: drop `let name = member.name.as_str();` in favor of `let name = &member.name;` so the typed `&DiskName` flows into the push sites and the `PreviewNote::PerDisk { name: ... }` constructors. `PreviewNote::PerDisk.name` is a `String` (display-only); keep it `String` and use `name.as_str().to_owned()` at the push sites so the note display contract is unchanged.
- Line 133: push `(member.name.clone(), member.by_id.clone())`.

Update slot preflight (`check_slot_one_available`, line 152-166): change `name: &str` to `name: &DiskName`. `format!("... {} ({}) ...", name, by_id, ...)` continues to work because `DiskName: Display`.

Update consumers:

- `enroll_key_file.rs:237`: slice signature `&[EnrollmentCandidate]` is unchanged at the type-alias level, but every per-element access now yields `(&DiskName, &ByIdPath)`. Trace the function body to switch any `&str` bindings.
- `enroll_key_file.rs:371`: same shape -- `execute_enrollment_for_candidates`.
- `enroll_key_file.rs:430`: dry-run preview storage `pub candidates: Vec<EnrollmentCandidate>` is automatically retyped by the alias.
- `enroll_key_file.rs:612`: `Vec::with_capacity` typing follows the alias.

Update the per-disk-mode planner (`plan_single_disk_enrollment`, ~line 194): change `name: &str` parameter to `&DiskName`. Caller at `add.rs:1212` drops `.as_str()`.

Sites at `enroll_key_file.rs:314, 387` (`let mn = mapper_name(name);`) already have the correct shape; once `name: &DiskName` flows through from `DiskEnrollAction.name`, no further change is needed.

Update the in-file `#[cfg(test)]` test module to use the `disk("...")` helper.

### Step 7 -- LUKS-label inline replacements (the 12 production sites)

For each site, replace the inline `format!("braid-{}", x)` with `luks_label_for(&x)`. Where the field expects `String` (the observed-label compare path), use `luks_label_for(&x).as_str()`. Where it expects `LuksLabel` (the new argv field), pass the `LuksLabel` directly. The 12 sites:

1. `add.rs:138` -- `let expected_label = luks_label_for(name);` (after Step 8 makes `name: &DiskName`).
2. `add.rs:479` -- `let label = luks_label_for(&target.name);` -- field receives `LuksLabel` after Step 3.
3. `add.rs:1093` -- `let label = luks_label_for(name);` -- passed as `&label` to `luks_format`.
4. `replace.rs:254` -- `let label = luks_label_for(&self.new_name);`.
5. `replace.rs:621` -- `let label = luks_label_for(&new_name);`.
6. `recover.rs:837` -- `let label = config::luks_label_for(&target.name);`.
7. `recover.rs:865-868` -- rewrite per the locked design:
   ```rust
   let expected_label = config::luks_label_for(&target.name);
   let fresh_conditional_suffix = format!(
       "{conditional_suffix} (the LUKS format command is also skipped at runtime if the disk already shows a LUKS header with the journaled UUID and the '{expected_label}' label)"
   );
   ```
8. `recover.rs:2123` -- `let expected_label = config::luks_label_for(&target.name);`.
9. `recover.rs:2220` -- same.
10. `recover.rs:2443` -- same.
11. `recover.rs:2585` -- same. Note: `expected_label` flows into `luks_format(..., &expected_label, ...)` at line 2593 -- now passes `&LuksLabel` to the retyped `luks_format`. Confirm the binding type is `LuksLabel`, not `&LuksLabel`.
12. `recover.rs:3063` -- `let expected_label = config::luks_label_for(new_name);` (after `new_name` shows up as `&DiskName` in `ReplaceFinishCtx`).

Comparison sites that read `label.as_deref() != Some(expected_label.as_str())` continue to work unchanged (LuksLabel exposes `as_str() -> &str`).

### Step 8 -- `cli/src/add.rs`: retype `validate_braid_preconditions`

`add.rs:132`:

```rust
fn validate_braid_preconditions(
    name: &DiskName,
    device: &str,
    label: Option<&str>,
    pool: &PoolState,
) -> Result<(), AddError> {
    let expected_label = luks_label_for(name);
    if label != Some(expected_label.as_str()) {
        ...
    }
}
```

Test callers at `add.rs:2788, 2811` use `validate_braid_preconditions(&disk("disk1"), ...)`.
Production caller at `add.rs:1849` drops `.as_str()`.

### Step 9 -- Convert remaining mapper-name callers

For each `mapper_name(x.as_str())` site, drop the `.as_str()`. For each `mapper_name(name)` where `name: &str`, change the surrounding function's parameter to `&DiskName` and propagate up. Sites listed in [Critical files] above; a comprehensive grep before/after the change confirms zero `mapper_name(...as_str())` and zero `mapper_name("literal")` remain outside of `disk("literal")` test helpers.

The `tui/probe.rs:35` straggler currently does `MapperName(format!("braid-{disk_name}"))` -- change to `mapper_name(disk_name)` and update the surrounding `fallback_disk_luks_lock(disk_name: &str, ...)` signature to `(disk_name: &DiskName, ...)`. The validated boundary lives one level up, in `build_disk_luks_states` at `cli/src/tui/probe.rs:106-137`:

- `build_disk_luks_states` iterates `disk_by_id: &HashMap<String, String>` (membership keys are owned strings on this code path). The HashMap key type stays `String` -- the TUI display maps (`disk_by_id`, `disk_luks_uuid`, `mounted_classification`, `disk_luks_states`) are display surfaces, not the typed-name domain, and retyping them would ripple across the TUI model.
- Instead, parse the key once at the call site: replace `disk_name` (the loop binding, `&String`) flowing into `fallback_disk_luks_lock` with a freshly parsed `DiskName`. Concretely:

  ```rust
  for (disk_name, by_id_path) in disk_by_id {
      let parsed_disk_name = DiskName::parse(disk_name)
          .expect("membership disk names are validated upstream");
      // ... mounted_classification lookup unchanged ...
      let (lock, underlying_present) = mounted_classification
          .get(disk_name)
          .cloned()
          .unwrap_or_else(|| {
              fallback_disk_luks_lock(
                  runner,
                  &parsed_disk_name,
                  by_id_path,
                  disk_luks_uuid.get(disk_name),
                  backing_path_resolver,
              )
          });
      // ... disk_luks_states.insert(disk_name.clone(), ...) unchanged ...
  }
  ```

  The `.expect(...)` is justified because `disk_by_id` is built from `PoolMembership` (which only holds validated `DiskName`s); if the assumption is ever invalidated, the panic message points directly at the boundary where the contract broke.

- The HashMap key lookups (`mounted_classification.get(disk_name)`, `disk_luks_uuid.get(disk_name)`, `disk_luks_states.insert(disk_name.clone(), ...)`) continue to use the original `&String` key -- the parsed `DiskName` is only used for the `fallback_disk_luks_lock` call.

### Step 10 -- Test fixtures

- `add.rs:7777` (`cloned_disk_probed`): change `label: Some(format!("braid-{name}"))` to `label: Some(luks_label_for(&disk(name)).as_str().to_owned())`. The fixture stores the *observed* label as `Option<String>`; the helper produces a `LuksLabel` whose inner field is private (per Step 1), so `as_str().to_owned()` is the correct accessor for the fixture's String slot. **Do not** write `luks_label_for(...).0` -- that will not compile.
- `test_fixtures/status.rs:656, 670`: replace inline `format!("braid-{name}")` mapper construction with `mapper_name(name)` (if `name: &DiskName`) or `mapper_name(&disk(name))` (if `name: &str`).
- `test_fixtures/remove.rs:258, 260`: same shape; replace `MapperName(format!("braid-{name}"))` with `mapper_name(name)` (typed) or `mapper_name(&disk(name))`.

### Step 11 -- Doc-comment updates in `journal.rs`

`journal.rs:39-43`: replace "derives the mapper as `mapper_name(&target.name)` and the label as `format!("braid-{name}")` at the call site that builds `CryptsetupLuksFormat`" with "derives the mapper via `config::mapper_name` and the label via `config::luks_label_for` at the call site that builds `CryptsetupLuksFormat`".

`journal.rs:72-74`: replace `"derived as `format!("braid-{name}")` at the format call site"` with `"derived via `config::luks_label_for` at the format call site"`.

`journal.rs:137-140`: same replacement.

`journal.rs:162` (the `luks_label` dropped-field reference in the deny-unknown-fields rationale) stays untouched -- it names a removed JSON field, not the helper.

### Step 12 -- Other non-obvious touch points

- `cli/src/discover.rs:1120`: `assert_eq!(label, "braid-é")` -- this test is about Unicode label tolerance, not the `braid-{name}` derivation. Stays untouched. Confirm by reading the surrounding test.
- `cli/src/lock.rs:65, 3188` -- doc comments that reference `mapper_name(...)` for `mapper` drift safety. No code change; the doc reference to the helper now means a typed helper, which only strengthens the comment.
- `cli/src/remove.rs:131, 2723, 2825` -- doc-comment references; no code change.

## Test plan

1. **Unit tests** (in `cli/src/config.rs`): one new `luks_label_for_disk` test, one updated `mapper_name_for_disk` test (signature change).
2. **No new tests beyond the unit pair.** Existing label substring assertions at `cli/src/replace.rs:3528, 4092, 5891` and `cli/src/add.rs:8139` continue to pin the byte-identical output through `Step::render_dry_run` and the argv builder. Drift detection is moot because the type system makes it impossible for `--label` argv to diverge from `luks_label_for(&name)`.
3. **No mock-injection drift test.** Centralization is the invariant; testing for drift would test the wrong thing.

## Verification

In order, after applying all edits:

1. `just clippy` -- catches stale imports, unused bindings, signature mismatches.
2. `just test-rust` -- the authoritative gate. Runs `cargo test --lib --bin braid --test golden_nixos_25_11 --test tty_guard`. Covers all `render_steps` substring assertions, the full argv builder roundtrip, and golden parser fixtures.
3. `cargo test --manifest-path cli/Cargo.toml -- mapper_name_for_disk luks_label_for_disk` -- narrow sanity check while iterating.
4. `cargo fmt --manifest-path cli/Cargo.toml` -- normalize whitespace.
5. **Skip `just test-vm`** -- the refactor produces byte-identical label/mapper strings; any escape from `test-rust` would be a latent bug surfaced by the type discipline, which `test-vm` would not catch any more reliably than the Rust suite.

## Risks and notes

- **`LuksLabel` privacy.** The inner field is `String`, not `pub String`. No serde derives are added (`CmdRequest` only derives `Debug, Clone, PartialEq, Eq`, so there is no real persistence path that needs them, and adding `Deserialize` would re-open the arbitrary-bytes construction door). Tuple-struct construction `LuksLabel(s)` is unreachable outside `types.rs`; the sole construction paths are `LuksLabel::for_disk(&DiskName)` and the thin `config::luks_label_for` re-export.
- **`MapperName` privacy is out of scope.** `MapperName(pub String)` has ~40 `.0` accesses across the codebase; tightening it the same way is a parallel cleanup, not part of this plan. A reader noticing the asymmetry can follow up.
- **`disk()` test helper.** Several test modules will need a `fn disk(name: &str) -> DiskName` helper to keep call sites readable. Define per module (not a public helper) -- the cost of `DiskName::parse("disk1").unwrap()` everywhere outweighs the cost of one short helper per test module.
- **Where the typed name was not previously present.** `DiskEnrollAction`, `EnrollmentCandidate`, `OpenPlan::to_unlock`, and `mount.rs::first_open_mapper` all move from `String` to `DiskName`. Any caller that constructs these by re-parsing the string from another source needs to ensure the source is itself a `DiskName` (or `DiskName::parse(...)` it once at the boundary, with a clear `.expect()` because the value came from validated upstream state).
- **Dead `"unknown"` fallback in `mount.rs:333`.** The current `unwrap_or("unknown")` is unreachable given the precondition check at `mount.rs:311`; Step 5 replaces it with `.expect(...)` describing the invariant. If a future change weakens the precondition, the panic message makes the violation visible at the right place.
- **`classify_mapper_ownership(name: &DiskName)`.** The function uses `name` only in error/log strings; the signature change is mechanical because `DiskName: Display` delegates to the inner string.
- **The `validate_braid_preconditions` `label: Option<&str>` parameter stays `&str`.** The argument is the *observed* probe label, not a constructed one, so it remains a borrowed string.
- **The dropped JSON field reference at `journal.rs:162`** (`luks_label`) is unrelated to the helper name; the new helper `luks_label_for` (note the suffix) does not collide.
- **No backwards-compatibility shim**: per project policy, the `mapper_name(&str)` form is replaced, not deprecated. If any in-flight branch depends on `&str` callers, rebase first.
