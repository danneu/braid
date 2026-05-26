# Plan: machine-parseable `last_scrub.started_at` in `braid status --json`

## Context

`docs/commands/status.md` advertises `braid status --json` as "a structured
report suitable for monitoring tools," but the scrub `last_scrub.started_at`
field serializes as a weekday/locale ctime string, e.g.
`"Mon Jan  1 00:00:00 2024"`. That shape is hostile to machine consumers
(weekday prefix, space-padded day, locale-dependent rendering) and inconsistent
with the ISO-8601 timestamps braid uses elsewhere (`journal.started_at`).

Today a single `String` feeds both the human status line and JSON
(`cli/src/status.rs`): `get_scrub_report` formats the parsed `ScrubTimestamp`
once via `format_scrub_timestamp` and stores it in `ScrubReport`, which serde
serializes directly.

**Decision (machine form): ISO-8601 with no offset**, e.g.
`"2026-02-23T10:00:00"`. btrfs prints this timestamp -- the original
`Scrub started:` time, or the `Scrub resumed:` time when a scrub was resumed --
via `localtime_r` + `strftime("%c")` (verified:
`reference/btrfs-progs/cmds/scrub.c:324-334`; braid's parser maps both lines
into `started_at` at `cli/src/parse/btrfs_scrub_status.rs:19-22`), so the value
is naive *local* wall-clock with no captured zone. A `Z`/UTC suffix
or epoch integer would fabricate an offset we do not have (and stamping the
*current* offset is wrong across DST). Naive ISO reports exactly what btrfs
knows, stays human-glanceable, and every ISO parser accepts it. The human
status line keeps its weekday form unchanged.

Intended outcome: JSON carries a stable, machine-parseable, honestly-labeled
`started_at`; the human renderer is unchanged; the format is documented.

## Approach

Split the one rendered string into two, mirroring the enum's existing
`journal_since` precedent (a `#[serde(skip)]` human-only field computed beside
the serialized one from the same `ScrubTimestamp`). No new types, no serde impl
on parse types.

### 1. New formatter (`cli/src/status.rs`, beside `format_scrub_timestamp`)

```rust
/// ISO-8601 local form for JSON consumers. Carries no offset: the
/// btrfs-reported scrub timestamp (`Scrub started` or `Scrub resumed`) is
/// naive local wall-clock (localtime_r/%c) and braid never captures the zone,
/// so a `Z`/UTC suffix would mislabel the value.
fn format_scrub_timestamp_iso(ts: &crate::parse::types::ScrubTimestamp) -> String {
    use time::macros::format_description;
    let fmt = format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]");
    ts.0.format(&fmt).unwrap_or_else(|_| "unknown".to_owned())
}
```

Do **not** reuse `format_scrub_timestamp_for_journalctl` (space separator, for
journalctl `--since`) or `membership.rs::format_rfc3339_utc_seconds` (emits a
literal `Z`). No existing helper produces a naive `T`-separated form.

### 2. `ScrubReport` field split (`cli/src/status.rs`, enum at ~line 142)

For each of `Finished` / `Aborted` / `Interrupted`:

```rust
Finished {
    started_at: String,        // ISO-8601 naive -> serialized to JSON
    #[serde(skip)]
    started_at_human: String,  // weekday ctime form -> human renderer only
    error_count: u64,
    #[serde(skip)]
    journal_since: String,
},
```

`started_at` keeps its name (it stays the documented JSON key); `started_at_human`
is the new skipped field. Per the project's no-backwards-compat rule, replacing
the JSON value in place is correct -- no parallel/legacy field.

### 3. Populate both in `get_scrub_report` (`cli/src/status.rs:756-791`)

Each arm sets `started_at: format_scrub_timestamp_iso(&started_at)` and
`started_at_human: format_scrub_timestamp(&started_at)`. `journal_since`
unchanged.

### 4. Human renderer (`cli/src/status.rs:1245-1276`)

Change the destructured field from `started_at` to `started_at_human` in the
`Finished` / `Aborted` / `Interrupted` arms. Output strings unchanged.

### 5. Docs (`docs/commands/status.md`, JSON section ~line 306-308)

Expand the `last_scrub` bullet to document `started_at` as ISO-8601
`YYYY-MM-DDTHH:MM:SS`, **offset-free, host-local wall-clock as reported by
btrfs** -- the btrfs-reported scrub timestamp (`Scrub started`, or `Scrub
resumed` after a resumed scrub), not necessarily the original start -- and
therefore not directly comparable to UTC fields like the pending-op
`started_at` (`...Z`). Leave the human "Last scrub:" examples (lines 130-138)
as-is.

## Tests

All in `cli/src/status.rs` unless noted. The contract is fully covered by Rust
unit tests; no VM test or fixture refresh is required.

- **JSON shape** (`scrub_report_json_{finished,aborted,interrupted}`, ~2910-2958):
  flip `assert_eq!(json["started_at"], "Mon Feb 23 10:00:00 2026")` to
  `"2026-02-23T10:00:00"`; add `started_at_human` to each construction.
- **Serde skip** (`scrub_report_json_skips_journal_since`, ~2961): add
  `started_at_human` to the constructed report, then strengthen the existing
  assertions to cover BOTH skipped fields -- assert the encoded JSON contains
  neither `journal_since` nor `started_at_human`, and that both decode back to
  `""`. (Today it checks only `journal_since`; without a `started_at_human` arm,
  a dropped `#[serde(skip)]` would leak the human form into JSON undetected --
  the field this plan exists to keep out of JSON.) Rename the test to reflect
  that it now guards both skipped fields.
- **Parse->report** (`status_scrub_{finished,finished_with_errors,aborted,interrupted}`,
  assertions at 2799/2826/2852/2878): replace
  `assert!(started_at.contains("Mon Feb 23"))` with an exact
  `assert_eq!(started_at, "2026-02-23T10:00:00")` plus
  `assert!(started_at_human.contains("Mon Feb 23"))` -- locks both forms.
- **Human render** (`human_scrub_*`, ~3016-3123): move `"Mon Feb 23 10:00:00 2026"`
  into the `started_at_human` field of each construction (set `started_at` to the
  ISO form); assertions on the human output are unchanged.
- **New**: a unit test asserting `format_scrub_timestamp_iso` on a known
  `ScrubTimestamp` yields exactly `"2026-02-23T10:00:00"` and contains no `Z`/`+`.

## Out of scope (do not touch)

- `cli/src/test_fixtures/status.rs` scrub mocks (`Scrub started: Mon Feb 23 ...`):
  raw btrfs *input* to the parser; btrfs's format is unchanged.
- TUI (`cli/src/tui/view/mod.rs`, `cli/src/tui/demo.rs`, `*.snap`): renders from
  `ScrubTimestamp` via its own `format_timestamp`, not from `ScrubReport`; unaffected.
- `format_scrub_timestamp_for_journalctl` / `journal_since` and
  `tests/repro/scrub-error-hint.py`: journalctl `--since` form, unchanged.
- `journal.started_at` (`now_iso`, UTC `Z`): already correct.

## Verification

- `just test-rust` -- exercises formatter, JSON shape, serde roundtrip,
  parse->report, and human render. This is the authoritative check.
- Optional sanity: `just test-vm braid-status` still passes
  (`tests/cli/braid-status.py` only asserts `last_scrub` is an object with a
  `state` field -- format-agnostic).
- No parser-fixture refresh: this is not a parser-critical tool-version change.
