# Plan: per-block-group-type Profile across `braid status` (human + JSON) and TUI

## Context

A code-review finding flagged that `braid status` and `braid doctor` do not
clearly distinguish a detection-only single-device pool from a healing-capable
RAID1 pool, so braid implicitly advertises self-healing on single-device pools.

An earlier revision of this plan proposed a single `Profile:` scalar line in
`braid status` derived from `summarize_df`'s data-profile join. That was too
narrow:

- `summarize_df` builds its profile string from `BtrfsBgType::Data` entries
  only (`cli/src/status.rs:644-654`), so the scalar undercounts the real
  redundancy story: btrfs profiles are per-block-group-type and metadata /
  system can diverge from data
  (`docs/internals/btrfs/balance-profiles.md:9-15`). E.g. the default 1-device
  bootstrap is `data=single` + `metadata=DUP` + `system=DUP`, so calling the
  whole pool "single" hides what DUP actually guarantees (same-disk copies,
  not disk redundancy).
- The `Allocation:` table already shows every per-type profile -- but as raw
  numbers without classification. The new section should be a glanceable
  summary built from the same data.
- The TUI consumes `pool.df_entries` directly (`cli/src/tui/model.rs:268`,
  `cli/src/tui/view/mod.rs:413`) and renders a per-type allocation table but
  no compact summary. Whatever classification we add belongs in both surfaces
  so wording stays in sync.

Intended outcome: `braid status` (human + `--json`) and `braid tui` all carry
a per-block-group-type Profile view. The human / TUI surfaces classify each
type's redundancy story with annotations that distinguish disk-redundant
(RAID1), same-disk-only (DUP), and no-redundancy (single) states; the JSON
surface exposes the raw btrfs profile names per type so downstream tooling
applies its own policy. The mixed case is called out so the operator knows
to run `braid doctor` for the remediation routing.

Decision 001 (`docs/design/decisions/001-btrfs-raid1.md:41`) frames single-disk
start as an intentional "incremental growth" feature, so this is a clarity
fix, not a safety bug. No new refusal, no new prompt.

## Scope

In scope:

1. Add a shared profile classifier (new module
   `cli/src/profile_summary.rs`) that takes block-group entries and returns a
   per-type `(profiles, redundancy class)` summary for Data, Metadata, and
   System.
2. Render a multi-line `Profile:` section in `braid status` human output,
   built from the shared classifier off `report.allocation`.
3. Render a compact `Profile` line in `braid tui`'s pool-info widget, built
   from the same classifier off `pool.df_entries`.
4. Replace `StatusReport.profile: Option<String>` with a structured per-type
   object (`{ data, metadata, system }` arrays of profile name strings) so
   `braid status --json` exposes the same shape facts as the human / TUI
   surfaces. The JSON layer carries the raw btrfs profile names only, not
   braid's `Redundancy` classification text; arrays are always in canonical
   `profile_display_order`.
5. Align the single-disk bootstrap success message in `braid add` (already in
   the previous revision; kept verbatim per reviewer direction).
6. Add a `check_system_profile_mismatch` wrapper in `cli/src/doctor.rs`
   (symmetric with the existing Data/Metadata wrappers) so the "run
   `braid doctor`" guidance in the new Profile section is true for all
   three rows.
7. Unit tests for the classifier; updated/new unit tests for the human
   formatter and the JSON shape; new TUI snapshot tests; doctor tests for
   the new System wrapper.
8. VM regression assertions in `tests/cli/braid-status.py`,
   `tests/cli/braid-add-disk.py`, and JSON assertions in
   `tests/cli/braid-status-rust.py`.
9. Docs: `docs/commands/status.md` (human + JSON) and `docs/commands/tui.md`.

Out of scope (considered and rejected):

- **Classification text in JSON.** The JSON payload carries btrfs profile
  facts only (raw names per type). braid's `Redundancy` enum
  (`Mirrored` / `SameDisk` / `NoRedundancy` / `Mixed` / `Unknown`) stays
  an internal CLI/TUI helper used to choose human suffixes; JSON
  consumers derive their own policy from the names (e.g. "treat
  `single` as no-redundancy"). Mixing classification language into the
  schema would lock braid's wording into the wire format.
- **Backwards-compatible JSON fallback.** braid is unreleased and
  explicitly does not maintain backwards-compatible JSON shapes
  (`AGENTS.md` "No backwards compatibility"). The old scalar
  `profile: Option<String>` field is replaced, not duplicated.
- **Net-new doctor logic.** `braid doctor` keeps its existing remediation
  routing for mixed Data and Metadata
  (`check_data_profile_mismatch` / `check_metadata_profile_mismatch` at
  `cli/src/doctor.rs:620-794`). The one new wrapper -- a System-profile
  mismatch check using the existing `check_profile_mismatch` helper
  (see Change 6 below) -- is the minimum needed to keep the "run `braid
  doctor` for the next step" guidance honest now that the Profile
  section makes System first-class.
- **README rewording, `monitoring-and-alerts.md` rewrite, pre-confirm bootstrap
  WARNING.** Same reasoning as the previous revisions.

## Changes

### 1. New shared classifier: `cli/src/profile_summary.rs`

Create a new module. Public surface:

```rust
/// Per-block-group-type redundancy summary for `braid status` and `braid tui`.
/// Shared so CLI and TUI classification stay in sync; rendering is per-caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileSummary {
    pub data: TypeProfile,
    pub metadata: TypeProfile,
    pub system: TypeProfile,
}

/// One block-group-type's redundancy classification plus the raw profile
/// names that produced it. Renderers join `profiles` and append a suffix
/// chosen from `class`; pairing the classification with the raw names
/// keeps a single source of truth for what to display and what category
/// to put it in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeProfile {
    pub profiles: Vec<String>,  // deduped, ordered by canonical domain order
                                // (see profile_display_order below), e.g.
                                // ["single", "RAID1"] -- not alphabetical.
    pub class: Redundancy,
}

/// Coarse redundancy category used to choose a renderer suffix.
/// `Copy` because a `Redundancy` is a tiny tag enum that renderers
/// and callers pattern-match against repeatedly; moving by value
/// keeps the call sites readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Redundancy {
    Mirrored,        // RAID1, RAID1C3, RAID1C4, RAID10
    SameDisk,        // DUP
    NoRedundancy,    // single, RAID0
    Mixed,           // more than one profile for this type
    Unknown,         // empty entries, or only RAID5/RAID6/BtrfsProfile::Unknown
                     // (which braid doesn't classify; renderers print the
                     // raw profile name verbatim).
}

/// Canonical display rank for braid: matches the variant order in
/// `BtrfsProfile` (`cli/src/parse/types.rs:49-77`): single=0, DUP=1,
/// RAID0=2, RAID1=3, RAID1C3=4, RAID1C4=5, RAID5=6, RAID6=7, RAID10=8,
/// every unrecognized name=255 (sentinel). The rank alone is not the
/// final sort order -- unknown names share rank 255, so the classifier
/// uses `(profile_display_order(name), first_seen_index)` as the sort
/// key with a *stable* sort. This guarantees:
///   - known profiles appear in canonical domain order;
///   - unrecognized names appear after the last known profile, in the
///     order btrfs first reported them, deduped by first occurrence.
fn profile_display_order(p: &str) -> u8 { ... }

/// Build a `ProfileSummary` from raw df entries (TUI path).
pub fn from_df_entries(entries: &[BtrfsDfEntry]) -> ProfileSummary { ... }

/// Build a `ProfileSummary` from the report's allocation list (status path).
/// `AllocationEntry.bg_type` is a `String` ("Data" / "Metadata" / "System");
/// strings that don't match those three names are skipped.
pub fn from_allocation(entries: &[AllocationEntry]) -> ProfileSummary { ... }
```

Profile-vec build algorithm (single source of truth; renderers do not
re-sort):

1. Walk the input entries in source order, recording each unique
   profile name with its first-seen index.
2. Stable-sort the unique names by
   `(profile_display_order(name), first_seen_index)`.
3. Return the resulting `Vec<String>`.

Do **not** use `BTreeSet` for dedupe -- it sorts by `Ord<&str>`,
which collapses unknown names into lexicographic order and loses
the "btrfs report order" guarantee the JSON / human / TUI surfaces
all rely on. The classifier tests below pin both invariants.

Classification rules (single source of truth for both renderers):

| Per-type profile set                  | `Redundancy`     | `tp.profiles` |
| ------------------------------------- | ---------------- | ------------- |
| `{}` (no entries)                     | `Unknown`        | `[]` |
| `{RAID1}`, `{RAID1C3}`, `{RAID1C4}`, `{RAID10}` | `Mirrored` | the one name |
| `{DUP}`                               | `SameDisk`       | `["DUP"]` |
| `{single}` or `{RAID0}`               | `NoRedundancy`   | the one name |
| `{RAID5}`, `{RAID6}`, `{BtrfsProfile::Unknown(_)}` | `Unknown` | the raw name |
| any set with >1 distinct profile      | `Mixed`          | every name, deduped by first occurrence, stable-sorted by `(profile_display_order(name), first_seen_index)` -- known profiles in canonical domain order, unknown names in btrfs-report order |

**Why RAID5/RAID6/Unknown are `Unknown`, not their own class.** braid
never produces RAID5 or RAID6 (`docs/design/decisions/001-btrfs-raid1.md`
documents the project's RAID1-only stance). Calling parity profiles
`Mirrored` would over-promise (the redundancy story is different);
calling them `NoRedundancy` would under-promise. `Unknown` with the
raw profile name preserved in `tp.profiles` lets the renderer print
exactly what btrfs reported (`RAID5`, `RAID6`, or whatever
`BtrfsProfile::Unknown("FOO")` carries) without any annotation -- the
operator sees the truth and decides what to do.

`Unknown` therefore renders two different shapes depending on input:
- empty `tp.profiles` -> `unknown` (no data; e.g. before first probe).
- non-empty `tp.profiles` -> the raw join, no suffix (e.g. `RAID5`).

Profile name strings come from `BtrfsProfile::Display` at
`cli/src/parse/types.rs:62-77` -- braid produces `"single"` (lowercase),
`"DUP"` (uppercase), `"RAID1"` (uppercase). The classifier uses these
canonical strings verbatim so callers can render them without re-casing.

Unit tests in the new module (each carries the required `// Intent` /
`// Why it exists` / `// Scenario` preamble per `AGENTS.md` /
`docs/dev/testing.md:11-22`):

- `summary_for_3disk_raid1_pool` -- df with Data=RAID1, Metadata=RAID1,
  System=RAID1 -> all three are `Mirrored` with `profiles == ["RAID1"]`.
- `summary_for_single_disk_pool` -- df with Data=single, Metadata=DUP,
  System=DUP -> Data is `NoRedundancy`, Metadata + System are `SameDisk`.
- `summary_for_mixed_data_profile` -- df with Data entries on both
  `single` and `RAID1` -> Data class is `Mixed`,
  `profiles == ["single", "RAID1"]` (canonical domain order, **not**
  alphabetical -- this matches every user-facing example and the
  `profile_display_order` helper).
- `summary_for_mixed_metadata_profile` -- df with Metadata entries on
  `DUP` and `RAID1` -> Metadata class is `Mixed`.
- `summary_omits_global_reserve` -- df includes a `GlobalReserve` entry;
  it must not appear in Data/Metadata/System.
- `summary_for_empty_df` -- empty entries -> all three are `Unknown`
  with `profiles == []`.
- `summary_for_raid0_data` -- df with Data=RAID0 only -> Data class is
  `NoRedundancy` with `profiles == ["RAID0"]`. Pins the RAID0 branch
  that callers easily forget.
- `summary_for_raid5_data_is_unknown` -- df with Data=RAID5 only ->
  Data class is `Unknown` with `profiles == ["RAID5"]`. Pins that
  parity profiles are surfaced verbatim rather than misclassified.
- `summary_for_unparsed_profile_is_unknown` -- df with
  `BtrfsProfile::Unknown("foo".into())` for Data -> Data class is
  `Unknown` with `profiles == ["foo"]` (raw display string preserved).
- `summary_preserves_unknown_tail_order` -- df with Data entries on
  `RAID1`, then `XENO` (unknown), then `FOOBAR` (unknown), then `RAID1`
  again. Expected `tp.profiles == ["RAID1", "XENO", "FOOBAR"]`: known
  RAID1 first by rank, then the two unknown names in btrfs's
  report order (`XENO` before `FOOBAR`), deduped by first
  occurrence. Pins the `(rank, first_seen_index)` tiebreak against
  a `BTreeSet` regression (which would produce
  `["RAID1", "FOOBAR", "XENO"]` alphabetically) and an unstable-sort
  regression (which could reorder them arbitrarily). Class is
  `Mixed` (more than one distinct profile).
- `from_allocation_matches_from_df_entries` -- given an allocation list
  built from a known df fixture, the two helpers return identical
  `ProfileSummary` (pins the contract between the two adapters).

### 2. New `Profile:` section in `format_status_human`

File: `cli/src/status.rs`, function `format_status_human` at lines 1012-1080.

Insertion point: **after** the `NotMounted` early return at lines 1062-1064,
immediately before the `Allocation:` block at line 1066. The not-mounted
return guarantees this section is suppressed when the pool is offline even if
a future code path populates `report.allocation` for a NotMounted report.

The block consumes `report.allocation`; if it is `None` or empty (e.g. the
df probe failed), the whole `Profile:` section is omitted -- the existing
"btrfs filesystem df failed -- ..." advisory at `cli/src/status.rs:434`
already explains the gap.

```rust
if let Some(ref alloc) = report.allocation
    && !alloc.is_empty()
{
    let summary = profile_summary::from_allocation(alloc);
    out.push_str("Profile:\n");
    for (label, tp) in [
        ("Data:    ", &summary.data),
        ("Metadata:", &summary.metadata),
        ("System:  ", &summary.system),
    ] {
        out.push_str(&format!(
            "  {label}  {}\n",
            format_type_profile_human(tp)
        ));
    }
}
```

`format_type_profile_human` lives next to `format_status_human` (or in
`profile_summary.rs` as a free function). Rendering rules:

Every renderer prints the actual profile name(s) from `tp.profiles`
followed by a class-specific suffix. Nothing is hardcoded -- the
classifier owns the names and the order; the renderer owns the suffix:

| `Redundancy`     | Suffix appended after the joined profile name(s)    | Example |
| ---------------- | --------------------------------------------------- | --- |
| `Mirrored`       | (no suffix)                                         | `RAID1` |
| `SameDisk`       | ` (same-disk copies; no disk redundancy)`           | `DUP (same-disk copies; no disk redundancy)` |
| `NoRedundancy`   | ` (no redundancy)`                                  | `single (no redundancy)` / `RAID0 (no redundancy)` |
| `Mixed`          | ` (not fully redundant)`                            | `single, RAID1 (not fully redundant)` |
| `Unknown` (empty `tp.profiles`) | n/a -- render literal `unknown`         | `unknown` |
| `Unknown` (non-empty `tp.profiles`) | (no suffix; raw join)                | `RAID5`, `RAID6`, or `foo` for `BtrfsProfile::Unknown("foo")` |

The joined name comes from `tp.profiles.join(", ")`, and `tp.profiles`
is already in canonical `profile_display_order` -- so renderers never
have to re-sort and the rendered string matches the test fixture string
byte-for-byte.

Resulting output (3-disk RAID1):

```
Profile:
  Data:      RAID1
  Metadata:  RAID1
  System:    RAID1
```

Single-disk bootstrap:

```
Profile:
  Data:      single (no redundancy)
  Metadata:  DUP (same-disk copies; no disk redundancy)
  System:    DUP (same-disk copies; no disk redundancy)
```

Mixed data profile (interrupted balance / degraded writes):

```
Profile:
  Data:      single, RAID1 (not fully redundant)
  Metadata:  RAID1
  System:    RAID1
```

Conventions:

- Label width matches the existing `Pool:`, `Status:`, `FSID:` columns
  (the leading two-space indent plus `Data: ` / `Metadata:` / `System: `
  is aligned so the values start in the same column).
- ASCII parentheses, not em-dash, per `AGENTS.md` "CLI Output Style".
- The `Allocation:` table that follows stays unchanged -- it carries the
  raw used/allocated bytes that the Profile section deliberately drops.

JSON output is **not** unchanged: see Change 3 below for the structured
replacement of `StatusReport.profile`. The human renderer and the JSON
serializer share the same `profile_summary` source of truth -- the human
side adds redundancy-class suffixes, the JSON side keeps only the raw
profile names.

### 3. Replace `StatusReport.profile` with a structured per-type object

File: `cli/src/status.rs` (field at line 58, producer at
`summarize_df` lines 650-683, caller mapping at line 524) and the new
`cli/src/profile_summary.rs` module.

The current `StatusReport.profile: Option<String>` is a join of Data
block-group profile names produced by `summarize_df` (e.g.
`"single"` or `"single, RAID1"`) -- the same scalar ambiguity the
human and TUI surfaces are fixing. Replace it with a structured
per-type object so JSON consumers see the same shape facts as the
human / TUI views.

Add a JSON DTO in `cli/src/profile_summary.rs`:

```rust
/// Per-block-group-type profile payload for `braid status --json`.
/// Carries the raw btrfs profile names braid observed (e.g. `single`,
/// `DUP`, `RAID1`), not braid's human-facing redundancy
/// classification. Pairing this with the human / TUI renderers'
/// `ProfileSummary` keeps a single source of truth for "what btrfs
/// reported"; downstream tooling applies its own redundancy policy.
/// Arrays are always in canonical `profile_display_order` so consumers
/// can rely on a stable sort.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProfileJson {
    pub data: Vec<String>,
    pub metadata: Vec<String>,
    pub system: Vec<String>,
}

impl From<&ProfileSummary> for ProfileJson {
    fn from(s: &ProfileSummary) -> Self {
        Self {
            data: s.data.profiles.clone(),
            metadata: s.metadata.profiles.clone(),
            system: s.system.profiles.clone(),
        }
    }
}
```

Field replacement in `StatusReport` (`cli/src/status.rs:58`):

```rust
// before
#[serde(skip_serializing_if = "Option::is_none")]
pub profile: Option<String>,

// after
#[serde(skip_serializing_if = "Option::is_none")]
pub profile: Option<ProfileJson>,
```

The `#[serde(skip_serializing_if = "Option::is_none")]` attribute is
preserved, so the `not_mounted` JSON shape (which the existing
`tests/cli/braid-status-rust.py:151` test pins as `"profile" not in s`)
stays correct -- the field is `None` whenever df data was not probed,
and serde omits the key entirely.

Producer change in `summarize_df` (`cli/src/status.rs:650-683`) and
its caller (`cli/src/status.rs:524`):

```rust
// summarize_df currently builds a `String` and dumps "unknown" when no
// profiles exist. Replace its profile-string production with a direct
// build of `ProfileJson` from the shared classifier, then drop the
// "unknown" branch -- per-type empty arrays carry the same meaning.

// status.rs:524, in DfSummary -> StatusReport mapping
profile: df_summary
    .as_ref()
    .map(|summary| ProfileJson::from(&summary.profile_summary)),
```

`DfSummary` (the local helper struct at `cli/src/status.rs:635`) gains
a `profile_summary: ProfileSummary` field and drops its existing
`profile: String` field. The `format_status_human` Profile section
added in Change 2 consumes `report.allocation` rather than
`DfSummary.profile_summary` so the human path stays a pure function of
the serialized report -- a property the existing tests already rely
on. The TUI path stays on `from_df_entries(&pool.df_entries)` per
Change 5.

JSON output shapes (per the agreed schema):

3-disk RAID1:
```json
"profile": {
  "data": ["RAID1"],
  "metadata": ["RAID1"],
  "system": ["RAID1"]
}
```

Single-disk bootstrap:
```json
"profile": {
  "data": ["single"],
  "metadata": ["DUP"],
  "system": ["DUP"]
}
```

Mixed-data after interrupted balance:
```json
"profile": {
  "data": ["single", "RAID1"],
  "metadata": ["RAID1"],
  "system": ["RAID1"]
}
```

Canonical order: every array is built by the shared classifier
algorithm described in Change 1 -- first-seen dedupe, then stable
sort by `(profile_display_order(name), first_seen_index)`. Known
profiles appear in domain order (`single`, `DUP`, `RAID0`, `RAID1`,
`RAID1C3`, `RAID1C4`, `RAID5`, `RAID6`, `RAID10`); unrecognized names
appear verbatim after the last known profile, in the order btrfs
first reported them. Arrays are **never** alphabetical; e.g. mixed
data serializes as `["single", "RAID1"]`, not `["RAID1", "single"]`,
and a `["RAID1", "XENO", "FOOBAR"]` tail stays in btrfs-report order
rather than getting re-sorted to `["RAID1", "FOOBAR", "XENO"]`.

Per-type empty arrays mean btrfs reported no block groups of that
type (the same case where the human renderer prints `unknown`). Empty
is the right JSON for "no data": braid does not invent sentinel
values and the rest of `StatusReport` follows the same convention.

Why replace rather than add a sibling field: braid is unreleased and
explicitly does not maintain backwards-compatible JSON shapes
(`AGENTS.md` "No backwards compatibility"). Leaving the old scalar
in place would recreate the dual-source-of-truth problem the human /
TUI changes fix.

Why no classification text in JSON: per the agreed schema, JSON
exposes btrfs profile facts. Downstream tooling derives its own
policy (e.g. "treat `single` as no-redundancy"); the `Redundancy`
enum stays an internal CLI/TUI helper for human prose, and the
strings `"no redundancy"`, `"same-disk copies; no disk redundancy"`,
and `"not fully redundant"` never appear in the JSON payload.

Mechanical sweep -- four kinds of call sites. The implementer must
update every match of:

```
rg -n 'profile: Some|profile: None|profile\.as_deref|profile\.is_none|"profile"\]' cli/src tests
```

and run that pattern again after the edit to confirm zero stragglers
outside the new `ProfileJson` / `Option<ProfileJson>` shape.

**(a) Test-module fixture writes** in `cli/src/status.rs`. About 20
sites currently write `profile: Some("RAID1".to_owned())` or
`profile: Some("single".to_owned())`. Verified inventory at
plan-write time: lines 1584, 1664, 1758, 1788, 1866, 1936, 2015,
2069, 2123, 2170, 2224, 2288, 2345, 2852, 2889, 2918, 4153, 4375,
4442, 4753.

**(b) Fixture-module writes** in `cli/src/test_fixtures/status.rs`.
This module is shared by other command modules' tests; missing it
breaks `cargo build` across half the test surface, not just
`status.rs`. Sites at plan-write time: line 615 (`Some("RAID1")`)
and line 640 (`None`). The `None` site is unaffected by the type
change; the `Some` site needs `ProfileJson::uniform("RAID1")` or a
per-type literal.

**(c) Read-side assertions** in `cli/src/status.rs` tests (build-time
break -- `.as_deref()` on `Option<ProfileJson>` returns
`Option<&ProfileJson>`, not `Option<&str>`):
  - lines 3043, 3140, 3173, 3211, 3253 -- currently
    `assert_eq!(built.report.profile.as_deref(), Some("RAID1"));`
    Rewrite as
    `assert_eq!(built.report.profile.as_ref().map(|p| p.data.clone()),
    Some(vec!["RAID1".to_owned()]));`
    (or the equivalent that matches what the fixture actually
    populates -- some of these sites currently assume a Data-only
    string; switch them to assert against `data` and add the
    Metadata/System assertions when the fixture supports them).
  - lines 3084, 3112 -- currently
    `assert!(built.report.profile.is_none());` Unchanged; `is_none()`
    still works on `Option<ProfileJson>`.

**(d) JSON shape assertion** at `cli/src/status.rs:1552`:
`assert_eq!(obj["profile"], "RAID1");` currently expects a scalar.
Rewrite to assert the object shape:

```rust
assert_eq!(
    obj["profile"],
    serde_json::json!({
        "data": ["RAID1"],
        "metadata": ["RAID1"],
        "system": ["RAID1"]
    })
);
```

The adjacent `assert_eq!(alloc[0]["profile"], "RAID1");` at line
1562 is the `AllocationEntry.profile: String` field (unchanged
scalar), not `StatusReport.profile`. Leave it alone.

**(e) VM test assertion** at `tests/cli/braid-status.py:82`:
`assert s["profile"] == "RAID1", ...` currently expects a scalar.
Rewrite to assert the structured shape and replace with the
following (matches the 3-disk RAID1 fixture this test exercises):

```python
assert s["profile"] == {
    "data": ["RAID1"],
    "metadata": ["RAID1"],
    "system": ["RAID1"],
}, f"Bad profile: {s['profile']!r}"
```

This is in addition to the `tests/cli/braid-status-rust.py` JSON
assertions added in Change 9 below -- `braid-status.py` already
captures `braid status --json` and was missed by an earlier draft
of this plan.

To keep parts (a)+(b) mechanical, add a `#[cfg(test)] impl
ProfileJson` helper in `cli/src/profile_summary.rs`:

```rust
#[cfg(test)]
impl ProfileJson {
    /// Test convenience: every block-group type carries the same
    /// single profile name. Mirrors the legacy `Some("RAID1".to_owned())`
    /// fixture form so the sweep stays a near-1:1 substitution.
    pub fn uniform(name: &str) -> Self {
        Self {
            data: vec![name.to_owned()],
            metadata: vec![name.to_owned()],
            system: vec![name.to_owned()],
        }
    }
}
```

The sweep is then `Some("RAID1".to_owned())` ->
`Some(ProfileJson::uniform("RAID1"))` and likewise for `"single"`.
Tests that care about per-type asymmetry (e.g. the new
`status_human_healthy_single` fixture from Change 4 below, which
needs Data=single + Metadata=DUP + System=DUP) construct
`ProfileJson { data: ..., metadata: ..., system: ... }` directly.

`profile: None` fixtures (e.g. `cli/src/status.rs:1731`,
`cli/src/status.rs:1838`, `cli/src/test_fixtures/status.rs:640`)
stay `None` -- the Option wrapper is unchanged, only the inner type
changed. Likewise `built.report.profile.is_none()` assertions stay
as-is.

External consumers verified at plan-write time: a `grep -rn
'StatusReport\b' cli/src/` finds zero non-defining references to
`StatusReport` outside `status.rs` itself; the TUI consumes
`pool.df_entries`, not `StatusReport.profile`; doctor consumes
`df.profiles_for(...)`, not `report.profile`. The only cross-module
fan-out is `cli/src/test_fixtures/status.rs` (sweep part (b)) and
`tests/cli/braid-status.py:82` (sweep part (e)) -- both covered
above.

### 4. Update / add `format_status_human` unit tests

File: `cli/src/status.rs`.

- `status_human_healthy_single` (line 1768): flip the
  `assert!(!human.contains("Profile:"))` at line 1834 into positive
  assertions for the three lines:
    - `Profile:\n` header present
    - `Data:      single (no redundancy)` present
    - `Metadata:  DUP (same-disk copies; no disk redundancy)` present
    - `System:    DUP (same-disk copies; no disk redundancy)` present
  Update the `allocation` fixture in the test to set Metadata + System
  profile to `"DUP"` (the test currently uses `"single"` for all three
  block group types at lines 1788-1801; that does not match what btrfs
  actually produces on a 1-device pool and would be a misleading fixture
  for this new behavior).
- `status_human_healthy_raid1` (line 1838): add positive assertions for
  `Profile:`, `Data:      RAID1`, `Metadata:  RAID1`, `System:    RAID1`,
  and a negative assertion that the output does **not** contain `no
  redundancy`, `same-disk copies`, or `not fully redundant`.
- `status_human_not_mounted` (line 1739): set `allocation:
  Some(vec![AllocationEntry { bg_type: "Data".into(), profile:
  "single".into(), ... }])` (currently `None`) and assert the output does
  **not** contain `Profile:`. This pins the formatter's NotMounted-return
  behavior even if a future code path populates `allocation` for a
  NotMounted report.
- **New test** `status_human_mixed_data_profile` (with the required
  three-line preamble): allocation has Data on both `single` and `RAID1`,
  Metadata + System on `RAID1`. Assert the output contains
  `Data:      single, RAID1 (not fully redundant)` and
  `Metadata:  RAID1`.

  Preamble form:

  ```rust
  // Intent: the human status formatter renders the Data row with the
  //   "not fully redundant" annotation when data block groups span more
  //   than one profile.
  // Why it exists: an exact-match "single" classifier would silently
  //   render a bare `Data:      single, RAID1` and lose the redundancy
  //   warning the operator needs after an interrupted balance or
  //   degraded writes.
  // Scenario: a 2-disk RAID1 was degraded long enough to allocate
  //   single-profile data chunks; the missing disk has since returned but
  //   the soft RAID1 balance has not yet drained those chunks.
  ```

- **New test** `status_human_unrecognized_profile_renders_verbatim`
  (with preamble): allocation has Data=RAID5, Metadata=RAID1,
  System=RAID1. Assert the output contains the exact substring
  `Data:      RAID5` and **does not** contain `unknown`,
  `no redundancy`, `same-disk copies`, or `not fully redundant` on the
  Data row. Pins the renderer-side contract that an unrecognized
  profile (class=`Unknown`, profiles=`["RAID5"]`) prints the raw name
  verbatim with no annotation -- the classifier tests alone do not
  catch a renderer that collapses non-empty Unknown to `unknown`.

- **New test** `status_human_missing_type_renders_unknown` (with
  preamble): allocation has only a Data=RAID1 entry; no Metadata or
  System entries. Assert the output contains `Data:      RAID1`,
  `Metadata:  unknown`, and `System:    unknown`. Pins the
  empty-`profiles` rendering branch as the literal `unknown` token
  (the only case where `unknown` is the right output).

**New JSON serde tests** (in `cli/src/status.rs` test module,
alongside the human-output tests; each with the three-line preamble):

- `status_json_healthy_single` -- build a `StatusReport` with
  Data=single, Metadata=DUP, System=DUP allocation; assert
  `serde_json::to_value(&report)["profile"]` equals
  `json!({"data": ["single"], "metadata": ["DUP"], "system": ["DUP"]})`.
- `status_json_healthy_raid1` -- 3-disk RAID1 fixture; JSON
  `profile` equals
  `{"data": ["RAID1"], "metadata": ["RAID1"], "system": ["RAID1"]}`.
- `status_json_mixed_data_profile` -- mixed Data fixture (Data on
  both `single` and `RAID1`, Metadata + System on `RAID1`); JSON
  `profile.data` equals exactly `["single", "RAID1"]`. Pins canonical
  domain order against an alphabetical-sort regression
  (`["RAID1", "single"]` would be wrong). Uses
  `assert_eq!(value["profile"]["data"], json!(["single", "RAID1"]))`
  so a future serializer that re-sorts the array fails the test.
- `status_json_not_mounted_omits_profile` -- not-mounted report
  (`profile: None`); assert `serde_json::to_value(&report)
  .get("profile").is_none()`. Pins the
  `#[serde(skip_serializing_if = "Option::is_none")]` contract that
  `tests/cli/braid-status-rust.py:151` already relies on.
- `status_json_no_classification_text` -- 3-disk RAID1 fixture;
  serialize, convert to string, and assert the JSON does **not**
  contain `"no redundancy"`, `"same-disk copies"`, or `"not fully
  redundant"`. Pins the schema decision that braid's human-facing
  classification strings stay out of the wire format.

### 5. Compact `Profile` line in the TUI

File: `cli/src/tui/view/mod.rs`, function `pool_info` at lines 324-393.

Insert a new line immediately after the `Path` line (currently at 327)
and before the optional Balance / Usage lines, built from
`profile_summary::from_df_entries(&pool.df_entries)`.

```rust
let summary = profile_summary::from_df_entries(&pool.df_entries);
let all_empty = summary.data.profiles.is_empty()
    && summary.metadata.profiles.is_empty()
    && summary.system.profiles.is_empty();
if !all_empty {
    lines.push(Line::from(vec![
        Span::styled("Profile    ", dim),
        Span::raw(format!(
            "data {} | meta {} | system {}",
            format_type_profile_tui(&summary.data),
            format_type_profile_tui(&summary.metadata),
            format_type_profile_tui(&summary.system),
        )),
    ]));
}
```

The omission condition checks `profiles.is_empty()`, not `class ==
Unknown`: under the edge policy, `Unknown` with a non-empty `profiles`
vec (e.g. `RAID5`) is real data the user should see, not a probe gap.
Only the all-three-empty case (no df data; pre-first-probe) suppresses
the line.

`format_type_profile_tui` is the TUI-side renderer (next to `pool_info` or
in `profile_summary.rs` as a free function). Compact rules:

| `Redundancy`     | Rendered                                |
| ---------------- | --------------------------------------- |
| `Mirrored`       | profile string verbatim (`RAID1`)       |
| `SameDisk`       | profile string verbatim (`DUP`)         |
| `NoRedundancy`   | profile string verbatim (`single`)      |
| `Mixed`          | `partial`                               |
| `Unknown` (empty `tp.profiles`)      | `unknown`                |
| `Unknown` (non-empty `tp.profiles`)  | `tp.profiles.join(", ")` verbatim (`RAID5`, `foo`, ...) |

The empty/non-empty split matches the CLI policy, so a pool with
`Data=RAID5` renders `data RAID5` (truthful) instead of `data unknown`
(misleading).

Resulting TUI rows:

```
Path       /mnt/storage
Profile    data RAID1 | meta RAID1 | system RAID1
Usage      29% 2.1 TiB / 7.3 TiB (Estimated)
```

```
Profile    data single | meta DUP | system DUP
Profile    data partial | meta RAID1 | system RAID1
```

The line is omitted only when **all three** `tp.profiles` vectors are
empty (no df data -- e.g. before the first probe completes). A
`tp.class == Unknown` with a non-empty `tp.profiles` (e.g. an
unrecognized `RAID5` entry) is real data, so its row is rendered
verbatim and the whole Profile line stays. The existing per-entry
allocation table at `cli/src/tui/view/mod.rs:395-431` stays untouched;
it remains the authoritative numbers view.

### 6. Add `check_system_profile_mismatch` in `braid doctor`

File: `cli/src/doctor.rs`.

The existing `check_profile_mismatch` helper at lines 620-691 is already
parameterized on `BtrfsBgType`; both `check_data_profile_mismatch` (line
783) and `check_metadata_profile_mismatch` (line 787) are thin wrappers
that pass `BtrfsBgType::Data` / `BtrfsBgType::Metadata` and a label.
System has no such wrapper -- so a `System: single, RAID1 (not fully
redundant)` row in `braid status` would point at a `braid doctor` that
prints nothing about System chunks. Add the symmetric wrapper:

```rust
fn check_system_profile_mismatch<R: CommandRunner>(
    ctx: &mut DoctorContext<'_, R>,
) -> CheckResult {
    check_profile_mismatch(
        ctx,
        BtrfsBgType::System,
        "system_profile_mismatch",
        "system",
    )
}
```

Wire it into the check vec at `cli/src/doctor.rs:1219-1228`, right after
`check_metadata_profile_mismatch`. Add the matching human label in the
`label` match at line 1256-1273:

```rust
"system_profile_mismatch" => "system profiles",
```

Doctor tests (each with the required preamble):

- `system_profile_clean_raid1_ok` -- df with System=RAID1 only -> `Ok`,
  message contains `RAID1`. Mirrors `data_profile_clean_raid1_ok` at
  `cli/src/doctor.rs:3078-3100`.
- `system_profile_mixed_warns` -- df with System on both `DUP` and
  `RAID1` -> `Warn`, message contains the mixed-profile breakdown and
  the soft RAID1 balance suggestion (the existing `check_profile_mismatch`
  produces an `-mconvert=raid1,soft` line; per
  `docs/internals/btrfs/balance-profiles.md:18-21`, btrfs converts
  system alongside metadata when `-m` is passed, so the suggestion is
  correct as-is).
- `system_profile_mismatch_recommends_replace_when_degraded` --
  symmetric with the data-side test at `cli/src/doctor.rs:3141-3166`:
  degraded pool + mixed system -> message says `replace` first, not
  balance.

Reason this is in scope despite "no new doctor logic" being out-of-scope
in earlier revisions: making System first-class in the Profile section
implicitly promises doctor coverage for the same row. Without this
wrapper, the docs row that says "Run `braid doctor` for the right next
step" is false for System.

### 7. Update / add TUI snapshot tests

File: `cli/src/tui/view/mod.rs` and `cli/src/tui/view/snapshots/`.

- The TUI uses `insta` snapshots via the `snap!` macro at
  `cli/src/tui/view/mod.rs:1449-1455`; snapshot files live in
  `cli/src/tui/view/snapshots/*.snap`.
- Existing pool-rendering snapshots (e.g. `snapshot_with_pool.snap` and
  any sibling that exercises `pool_info`) will gain the new `Profile`
  line. Update those snapshots via `cargo insta review` once the
  implementation is in place -- do not hand-edit `.snap` files.
- Add five new snapshot tests (each with the standard three-line
  preamble):
    - `tui_pool_info_3disk_raid1` -- df fixture with Data/Metadata/System
      = RAID1; snapshot contains `Profile    data RAID1 | meta RAID1 |
      system RAID1`.
    - `tui_pool_info_single_disk` -- df fixture with Data=single,
      Metadata=DUP, System=DUP; snapshot contains
      `Profile    data single | meta DUP | system DUP`.
    - `tui_pool_info_mixed_data` -- df fixture with Data on both single
      and RAID1; snapshot contains `data partial`.
    - `tui_pool_info_unrecognized_profile_renders_verbatim` -- df
      fixture with Data=RAID5, Metadata=RAID1, System=RAID1; snapshot
      contains `data RAID5 | meta RAID1 | system RAID1`. Pins that an
      `Unknown` class with a non-empty raw name renders the name
      verbatim, not the `unknown` token. Symmetric with the CLI
      `status_human_unrecognized_profile_renders_verbatim` test.
    - `tui_pool_info_missing_type_renders_unknown` -- df fixture with
      Data=RAID1 only, no Metadata or System entries; snapshot
      contains `data RAID1 | meta unknown | system unknown`. Pins the
      empty-`profiles` branch as the literal `unknown` token. Symmetric
      with the CLI `status_human_missing_type_renders_unknown` test.

### 8. Single-disk bootstrap message in `braid add`

File: `cli/src/add.rs:1343-1352`.

Replace:

```rust
eprintln!("Pool created and mounted at {}", mount_point);
```

with:

```rust
eprintln!(
    "Pool created (data single; metadata/system DUP -- no RAID1 disk redundancy) and mounted at {}",
    mount_point
);
```

Rationale for the wording: the single-disk bootstrap command is
`mkfs.btrfs -d single -m dup -O block-group-tree <device>`
(`cli/src/cmd.rs:125-128`), so the resulting pool is data=single +
metadata=DUP + system=DUP. The previous draft's `"single profile"` was
the same scalar ambiguity this plan is fixing -- the per-type form
matches what `braid status` will now display and what `mkfs` actually
produced.

### 9. VM regression assertions

**File:** `tests/cli/braid-add-disk.py`, Phase 1.

After `add1_err = machine.succeed("cat /tmp/add1.err")` at line 40, add:

```python
bootstrap_line = "Pool created (data single; metadata/system DUP -- no RAID1 disk redundancy) and mounted at /mnt/storage"
assert bootstrap_line in add1_err, (
    f"expected single-disk bootstrap message in stderr, got: {add1_err!r}"
)
```

**File:** `tests/cli/braid-status.py`, Phase 1 (lines 47-69).

This test also currently has a scalar JSON assertion at line 82
(`assert s["profile"] == "RAID1"`); the rewrite to the structured
RAID1 object is documented in Change 3, sweep part (e). The
verification step `just test-vm braid-status` exercises both -- if
the line-82 rewrite is missed, this VM test fails at runtime even
when the binary builds.

After the existing `RAID1` assertion at line 54, add:

```python
assert "Profile:" in output, f"Expected 'Profile:' header:\n{output}"
assert "Data:      RAID1" in output, f"Expected 'Data:      RAID1':\n{output}"
assert "Metadata:  RAID1" in output, f"Expected 'Metadata:  RAID1':\n{output}"
assert "System:    RAID1" in output, f"Expected 'System:    RAID1':\n{output}"
assert "no redundancy" not in output, (
    f"3-disk RAID1 pool must not report 'no redundancy':\n{output}"
)
```

This pins the per-type rendering against live tool output for the
canonical 3-disk RAID1 pool.

**File:** `tests/cli/braid-status-rust.py`, Phase 1 single-disk subtest
(lines 43-54).

That test already captures `braid status` from a real single-disk pool
built via the live `braid add` -> `mkfs.btrfs -d single -m dup` ->
mount path (`cli/src/cmd.rs:125-128`). After the existing
`assert "single" in output` at line 50, add the per-type assertions
that pin every row the new Profile section renders against the live
mkfs output:

```python
assert "Profile:" in output, f"Expected 'Profile:' header:\n{output}"
assert "Data:      single (no redundancy)" in output, (
    f"Expected 'Data:      single (no redundancy)':\n{output}"
)
assert "Metadata:  DUP (same-disk copies; no disk redundancy)" in output, (
    f"Expected DUP metadata row:\n{output}"
)
assert "System:    DUP (same-disk copies; no disk redundancy)" in output, (
    f"Expected DUP system row:\n{output}"
)
```

The same test's Phase 2 "Healthy RAID1 summary" subtest (lines 64-) is
already covered by the parallel human-text additions in
`braid-status.py`; no new human-text assertions needed there because
the two tests render the same Profile section against the same
canonical 3-disk RAID1 fixture.

**JSON shape assertions** (also in `tests/cli/braid-status-rust.py`).
This test is the only VM test that already captures `braid status
--json` (Phase 2 "Healthy JSON" subtest at lines 86-100, Phase 3
"Degraded JSON" at 128-135, Phase 4 "Not mounted JSON" at 145-152).
The Phase 4 subtest already asserts `"profile" not in s` -- that
assertion stays correct and confirms the
`skip_serializing_if = "Option::is_none"` contract under the new
type.

Add a new Phase 1 JSON subtest (no JSON capture currently exists in
Phase 1) immediately after the human-text single-disk subtest, and
extend the existing Phase 2 "Healthy JSON" subtest:

```python
# new Phase 1 subtest, after "Single-disk summary"
with subtest("Single-disk JSON"):
    raw = machine.succeed(rust_status("--json"))
    s = json.loads(raw)
    assert s["profile"] == {
        "data": ["single"],
        "metadata": ["DUP"],
        "system": ["DUP"],
    }, f"single-disk JSON profile mismatch: {s['profile']!r}"
```

```python
# additions inside the existing Phase 2 "Healthy JSON" subtest
# (after the existing disk assertions)
assert s["profile"] == {
    "data": ["RAID1"],
    "metadata": ["RAID1"],
    "system": ["RAID1"],
}, f"3-disk RAID1 JSON profile mismatch: {s['profile']!r}"
```

These two JSON blocks pin the structured shape against live
btrfs-progs output for both the single-disk and 3-disk RAID1 cases.
The Phase 3 "Degraded JSON" subtest does **not** need a profile
assertion -- a degraded RAID1 pool reports the same profile shape as
a healthy RAID1 (RAID1 profile names on each type), and bg-type
membership during degraded operation is already covered by the
human-text RAID1 assertion in Phase 2.

Together these VM assertion blocks cover the single-disk and 3-disk
RAID1 cases against live tool output in both human and JSON forms.
The mixed-data case remains unit-test + TUI-snapshot only -- no
existing VM test constructs a mid-balance mixed-profile pool, and
building one is out of scope here. The unit-level
`status_json_mixed_data_profile` test in Change 4 covers the
canonical-order regression risk that a live mixed pool would
otherwise have surfaced.

### 10. Documentation updates

**File:** `docs/commands/status.md`, "Pool summary" section at lines 36-49.

Replace the example:

```
Pool:     /mnt/storage
Status:   intact
```

with:

```
Pool:     /mnt/storage
Status:   intact
FSID:     <uuid>
Profile:
  Data:      RAID1
  Metadata:  RAID1
  System:    RAID1
```

Then add a "Profile section" subsection after the existing "Status
values" table. Anchor the wording to per-block-group-type semantics
(citing `docs/internals/btrfs/balance-profiles.md` for the per-type
design) and route mixed-profile remediation through `braid doctor`:

| Per-type rendering                                      | Meaning |
| ------------------------------------------------------- | --- |
| `RAID1` (also `RAID1C3`, `RAID1C4`, `RAID10`)           | Mirrored across drives; reads self-heal from the redundant copy. |
| `DUP (same-disk copies; no disk redundancy)`            | Two copies on the same physical device (the default metadata/system profile on a 1-device pool). Survives bit-rot, not device failure. |
| `single (no redundancy)` (also `RAID0 (no redundancy)`) | One copy across the affected block groups. Checksums detect bit-rot, but corruption cannot be repaired. |
| `single, RAID1 (not fully redundant)`                   | Block groups for this type span more than one profile -- typically after an interrupted balance or degraded writes. Run `braid doctor` for the right next step; doctor recommends a soft RAID1 balance on a healthy pool and `braid replace` first on a degraded pool. |
| `unknown`                                               | No block groups of this type were reported (rare; check `braid status` advisories for a df probe failure). |
| `RAID5`, `RAID6`, or any unrecognized name              | braid does not classify parity profiles or future btrfs profiles. The raw profile name is shown verbatim with no annotation so the operator can make their own call; braid only ever produces `single`, `DUP`, and `RAID1`. |

Note the whole section is omitted when the pool is `not mounted` or the
df probe failed.

**File:** `docs/commands/status.md`, JSON output section.

Find the existing `braid status --json` documentation (or add one if
absent; the file currently documents only human output). Document the
new structured `profile` object explicitly:

> **`profile`** (`object` or absent) -- present whenever btrfs reports
> block-group allocation, omitted when the pool is not mounted or
> `btrfs filesystem df` failed. The object always has three keys
> -- `data`, `metadata`, `system` -- each holding an array of the
> btrfs profile names that block group type is allocated on. The
> values are the raw btrfs profile names (`single`, `DUP`, `RAID0`,
> `RAID1`, `RAID1C3`, `RAID1C4`, `RAID5`, `RAID6`, `RAID10`, or an
> unrecognized name verbatim). Per-type arrays are sorted in canonical
> domain order -- never alphabetical -- so consumers can rely on a
> stable shape: `["single", "RAID1"]`, **not** `["RAID1", "single"]`.
> An empty array for a type means btrfs reported no block groups of
> that type.

Add three JSON examples showing the same three canonical pool states
as the human section:

```json
// 3-disk RAID1
"profile": {
  "data": ["RAID1"],
  "metadata": ["RAID1"],
  "system": ["RAID1"]
}
```

```json
// single-disk bootstrap
"profile": {
  "data": ["single"],
  "metadata": ["DUP"],
  "system": ["DUP"]
}
```

```json
// mixed data after interrupted balance
"profile": {
  "data": ["single", "RAID1"],
  "metadata": ["RAID1"],
  "system": ["RAID1"]
}
```

Call out the schema decision explicitly:

> braid's human-facing redundancy classification (the
> `(no redundancy)` / `(same-disk copies; no disk redundancy)` /
> `(not fully redundant)` annotations from the human output) does
> **not** appear in JSON. The JSON payload carries only the btrfs
> profile names braid observed; consumers apply their own policy
> (e.g. "treat `single` as no-redundancy").

**File:** `docs/commands/tui.md`, expand the main-view description (line 52).

Replace:

> "**Main view** -- pool status, mount point, capacity bar, RAID profile, scrub state, balance state, and active alerts."

with a section that describes the compact `Profile` line specifically:

> "**Main view** -- pool status, mount point, the `Profile` summary
> (`data <X> | meta <Y> | system <Z>`, where each is the profile name
> verbatim for a single recognized profile (`RAID1`, `DUP`, `single`,
> ...), `partial` when that block group type spans more than one
> profile, the raw profile name verbatim for an unrecognized profile
> like `RAID5`, or `unknown` only when no block groups of that type
> were reported), capacity bar, scrub state, balance state, and active
> alerts."

## Verification

The insta cycle is "test, accept, retest" per `docs/dev/tui-snapshots.md:17-41`:
the first test run is expected to fail on new/changed snapshots, snapshots are
reviewed and accepted, then a second test run is the passing gate.

0. **Mechanical fixture sweep first.** The
   `StatusReport.profile` type change (Change 3) will not compile until
   every `profile: Some("...".to_owned())` initializer in
   `cli/src/status.rs` tests is rewritten to use
   `ProfileJson::uniform(...)` or a per-type literal. Run the sweep
   before running tests; `cargo build` fails fast and identifies any
   missed sites.
1. **First `just test-rust` -- expected to fail on snapshots.** New
   `profile_summary` unit tests, new JSON serde tests, and updated
   `format_status_human` / doctor tests should pass; existing TUI
   snapshots that exercise `pool_info` (e.g. `snapshot_with_pool.snap`)
   will fail with `.snap.new` files because the new `Profile` line is
   now in the rendered output. New TUI snapshot tests
   (`tui_pool_info_3disk_raid1` etc.) will fail on first run for the
   same reason -- no baseline exists.
2. **`cargo insta review`** (or `cargo insta accept` if confident) --
   walk the pending `.snap.new` diffs, confirm each one only added the
   `Profile` line, and accept. Commit the regenerated and new `.snap`
   files alongside the code change.
3. **Second `just test-rust` -- the passing gate.** With snapshots
   accepted, all unit tests pass and any future drift in the rendered
   `Profile` line will fail this step.
4. `just test-vm braid-status braid-status-rust braid-add-disk` --
   exercises the three updated VM tests against live tool output
   (3-disk RAID1 status, single-disk + RAID1 status against the live
   mkfs / parser path, and the single-disk bootstrap stderr).
5. `mdbook build docs` -- confirms `docs/commands/status.md` and
   `docs/commands/tui.md` parse and cross-links validate
   (per Decision 5 / `docs/book.toml` linkcheck).
6. Manual smoke (after merge, on a dev VM):
    - `sudo braid add disk1=/dev/...` -> stderr last line reads `Pool
      created (data single; metadata/system DUP -- no RAID1 disk
      redundancy) and mounted at /mnt/storage`.
    - `sudo braid status` on the single-disk pool -> contains
      `Data:      single (no redundancy)` and
      `Metadata:  DUP (same-disk copies; no disk redundancy)`.
    - `sudo braid add disk2=/dev/...` then a manual
      `btrfs balance start -mconvert=raid1 ...` -> `braid status` shows
      `Data:      RAID1` and `Metadata:  RAID1`.
    - `sudo braid tui` -> the new `Profile` row appears between `Path`
      and `Usage`.
    - `sudo braid status --json | jq .profile` on the single-disk pool
      -> `{ "data": ["single"], "metadata": ["DUP"], "system": ["DUP"] }`.
    - After adding the 2nd disk and running a manual
      `btrfs balance start -dconvert=raid1 -mconvert=raid1 ...`,
      `sudo braid status --json | jq .profile` -> all three arrays are
      `["RAID1"]`.

Do **not** run `cargo fmt`, `cargo fmt --check`, `just fmt`, or any other
formatter wrapper -- AGENTS.md forbids autonomous formatter runs
(`AGENTS.md:255-258`). Hand-format any new lines.

## Files touched

- **New:** `cli/src/profile_summary.rs` -- shared classifier
  (`ProfileSummary`, `TypeProfile`, `Redundancy`), JSON DTO
  (`ProfileJson`), `profile_display_order` helper, `from_df_entries` /
  `from_allocation` constructors, `#[cfg(test)] ProfileJson::uniform`
  helper, plus unit tests for the classifier.
- `cli/src/lib.rs` -- declare the new module.
- `cli/src/status.rs` -- `StatusReport.profile` field type change
  (`Option<String>` -> `Option<ProfileJson>`), `DfSummary` field
  swap, `summarize_df` producer update, new `Profile:` block in
  `format_status_human`, three human-test updates (single / raid1 /
  not-mounted) + three new human tests (mixed data,
  unrecognized-profile-verbatim, missing-type-unknown), five new
  serde tests (single, raid1, mixed-data canonical order, not-mounted
  omits profile, no classification text), JSON-shape assertion
  rewrite at line 1552 (`obj["profile"]` scalar -> object equality),
  read-side `.as_deref()` rewrite at lines 3043, 3140, 3173, 3211,
  3253 (assert against `ProfileJson.data` instead of `&str`), and
  mechanical sweep of ~20 `profile: Some("...".to_owned())` fixture
  initializers across the test module to use
  `ProfileJson::uniform(...)` or per-type literals. Full sweep
  pattern: `rg -n 'profile: Some|profile: None|profile\.as_deref|profile\.is_none|"profile"\]' cli/src tests`.
- `cli/src/test_fixtures/status.rs` -- shared fixture module also
  has a `profile: Some("RAID1".to_owned())` site at line 615;
  rewrite to `ProfileJson::uniform("RAID1")`. The `profile: None`
  site at line 640 stays as-is. Missing this file breaks `cargo
  build` across every command module that depends on it.
- `cli/src/tui/view/mod.rs` -- new `Profile` line in `pool_info`, five
  new snapshot tests (3-disk RAID1, single-disk, mixed-data,
  unrecognized-profile-verbatim, missing-type-unknown).
- `cli/src/tui/view/snapshots/*.snap` -- updated and new snapshots
  (regenerated via `cargo insta review`).
- `cli/src/doctor.rs` -- new `check_system_profile_mismatch` wrapper,
  registered in the check vec and the human label map, plus three new
  doctor tests.
- `cli/src/add.rs` -- one `eprintln!` string change.
- `tests/cli/braid-add-disk.py` -- one new assertion in Phase 1.
- `tests/cli/braid-status.py` -- per-type human assertions in Phase 1
  (3-disk RAID1), plus rewrite of the existing scalar JSON
  assertion at line 82 (`s["profile"] == "RAID1"`) to the structured
  RAID1 object `{ "data": ["RAID1"], "metadata": ["RAID1"], "system":
  ["RAID1"] }`. Missing this rewrite breaks the test at runtime even
  though the binary compiles, because the scalar `"RAID1"` never
  matches the new object shape.
- `tests/cli/braid-status-rust.py` -- per-type human assertions in
  Phase 1 (single-disk live mkfs path), new "Single-disk JSON" subtest
  in Phase 1, and JSON profile assertions added to the existing
  "Healthy JSON" subtest in Phase 2. The existing Phase 4
  "Not mounted JSON" `"profile" not in s` assertion stays as-is and
  serves as a passive regression check on the
  `skip_serializing_if = "Option::is_none"` contract.
- `docs/commands/status.md` -- example update, new "Profile section"
  subsection (human), and new JSON-shape documentation with three
  examples and the explicit no-classification-text callout.
- `docs/commands/tui.md` -- expanded main-view description.

`StatusReport`'s JSON schema **does** change: the scalar `profile:
Option<String>` is replaced by a structured `profile:
Option<ProfileJson>` (`{ data, metadata, system }` arrays). braid is
unreleased, so no migration path or compatibility shim is required
(per `AGENTS.md` "No backwards compatibility"). No principle /
decision docs are touched. The only doctor surface change is the
System wrapper symmetric with the existing Data/Metadata wrappers.
