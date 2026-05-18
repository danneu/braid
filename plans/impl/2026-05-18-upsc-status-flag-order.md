# Preserve `upsc` emission order for `ups.status` flags

## Context

`upsc ups` emits `ups.status: OB LB`, but `braid ups status` renders
`Status: LB OB`. The divergence comes from four sites that lex-sort
tokens out of a `HashSet<UpsStatusFlag>`:

- `cli/src/ups.rs:273-280` -- `format_status()` for the human CLI.
- `cli/src/parse/types.rs:580-597` -- `serialize_status_flags_sorted`
  hooked into `--json` via `#[serde(serialize_with = ...)]` (commit
  `bee9bb1`, plan at
  `plans/impl/2026-05-14-ups-json-status-flags-ordering.md`).
- `cli/src/tui/view/mod.rs:240-247` -- `format_ups_flags()` for the
  TUI Data tab.
- `cli/src/tui/browse/view.rs:298-308` -- `ups_status_lines()` for
  the TUI Browse > NUT > Status surface (`flags.sort()` at line 306).

A review finding proposed deriving `Ord` on the enum and dropping the
sort in `format_status`. That fix is partial -- it touches one of
three sort sites and still picks a static order that diverges from
NUT for combinations like `CAL+OL` (NUT emits `CAL OL`; the enum order
puts `OL CAL`).

The cleaner answer: stop synthesizing an order. NUT's `upsc` does not
reorder anything (`reference/nut/clients/upsc.c:141` is a single
`printf`); per-driver `status_set` call sequence is deterministic
(`reference/nut/drivers/usbhid-ups.c:2242-2513`, dummy-ups passthrough
at `reference/nut/drivers/dummy-ups.c:826-834` via
`str_add_unique_token` in `reference/nut/common/common.c:2125`). So
preserving `upsc`'s emission order in braid's parser produces output
that byte-matches the `ups.status:` line in `upsc ups` for any given
driver+state, and is stable across reruns -- the existing
"stable shape for scripts" contract (`manual/commands/ups-status.md:92`,
`docs/decisions/020-ups-integration.md:61`) is preserved.

Intended outcome: `braid ups status` and `braid ups status --json`
emit `status_flags` in the order `upsc` printed them, and both TUI
surfaces (Data tab and Browse > NUT > Status) mirror that order.
`snapshot_human_lowbattery.snap:6` flips from `Status: LB OB` to
`Status: OB LB`. All four sort sites collapse to zero.

## Design

Switch the parsed container from `HashSet<UpsStatusFlag>` to
`Vec<UpsStatusFlag>`. The parser pushes each parsed token in `upsc`
emission order, deduplicating on push so malformed input
(`ups.status: OL OL OB`) collapses to set semantics. All four render
sites iterate the `Vec` verbatim, no sort. Membership-test consumers
(`.contains()`, `.iter().any(...)`, `.is_empty()`) work unchanged
because `Vec` has matching methods at O(n) on n<=17, which is trivial.

The `serialize_status_flags_sorted` helper at
`cli/src/parse/types.rs:577-597` is deleted -- with `Vec` storage,
serde walks insertion order naturally and lex-sorting would
re-introduce the very divergence we are removing. The
`#[serde(serialize_with = ...)]` attribute on the field comes off.

The doc comment on `status_flags` becomes load-bearing for the new
contract (per the AGENTS.md doc-comment rule: capture
invariant/coupling that the signature does not).

## Changes

### 1. Parser switches container to order-preserving Vec

`cli/src/parse/upsc.rs:46`

```rust
let mut status_flags: Vec<UpsStatusFlag> = Vec::new();
```

`cli/src/parse/upsc.rs:71-75`

```rust
"ups.status" => {
    for tok in value.split_ascii_whitespace() {
        let flag = UpsStatusFlag::from_token(tok);
        if !status_flags.contains(&flag) {
            status_flags.push(flag);
        }
    }
}
```

Rationale: `Vec::dedup` only collapses *consecutive* duplicates;
`OL OB OL` would survive. The infallible-parser contract
(`cli/src/parse/upsc.rs:11-13`) means malformed input must still
yield set semantics, so dedupe-on-push is the correct shape. O(n^2)
on a maximum of ~17 tokens is irrelevant.

### 2. Storage type and dropped JSON-sort hook

`cli/src/parse/types.rs:651`

```rust
/// Flags from `ups.status`, in `upsc` emission order, deduplicated
/// on push. Order is the script-facing contract: human render and
/// `--json` both iterate this Vec verbatim, matching the
/// `ups.status:` line in `upsc ups` byte-for-byte. Membership tests
/// (`.contains()`, `is_critical()`, `is_on_battery()`) treat the
/// Vec as a set; dedupe-on-push keeps those calls honest.
pub status_flags: Vec<UpsStatusFlag>,
```

Drop the `#[serde(serialize_with = "serialize_status_flags_sorted")]`
attribute. Delete `serialize_status_flags_sorted` and its doc comment
(`cli/src/parse/types.rs:577-597`).

The existing `impl Serialize for UpsStatusFlag` at
`cli/src/parse/types.rs:568-575` stays -- it emits the per-flag token
via `as_token()`; only the array-level ordering changes.

`is_critical` and `is_on_battery` (`cli/src/parse/types.rs:553-565`)
require no edits.

### 3. Render sites: drop the sorts

`cli/src/ups.rs:273-280` becomes:

```rust
fn format_status(flags: &[UpsStatusFlag]) -> String {
    if flags.is_empty() {
        return "(unknown -- ups.status missing)".to_owned();
    }
    flags
        .iter()
        .map(UpsStatusFlag::as_token)
        .collect::<Vec<_>>()
        .join(" ")
}
```

Signature flips from `&HashSet<UpsStatusFlag>` to
`&[UpsStatusFlag]` for caller flexibility (the single caller at
`cli/src/ups.rs:190` passes `&parsed.status_flags`, which auto-derefs
from `Vec` to slice).

`cli/src/tui/view/mod.rs:240-247` mirrors the same change:

```rust
fn format_ups_flags(flags: &[UpsStatusFlag]) -> String {
    if flags.is_empty() {
        return "--".into();
    }
    flags
        .iter()
        .map(UpsStatusFlag::as_token)
        .collect::<Vec<_>>()
        .join(" ")
}
```

`cli/src/tui/view/mod.rs:226-237` -- `ups_severity_color` signature
flips from `&HashSet<UpsStatusFlag>` to `&[UpsStatusFlag]`. Body
unchanged (`.iter().any`, `.contains` work on slice).

`cli/src/tui/browse/view.rs:298-308` -- `ups_status_lines()` body
drops its `flags.sort()` at line 306:

```rust
let flags = if snapshot.flags.is_empty() {
    "--".to_owned()
} else {
    snapshot
        .flags
        .iter()
        .map(|f| f.as_token())
        .collect::<Vec<_>>()
        .join(" ")
};
```

Signature unchanged -- it takes `Option<&UpsSnapshot>` and the
`snapshot.flags` access auto-derefs from `Vec` to slice. This is
the fourth sort site that the original three-site list missed.

### 4. TUI snapshot field type

`cli/src/tui/model.rs:127`

```rust
pub flags: Vec<UpsStatusFlag>,
```

`cli/src/tui/probe.rs:769` -- the query-failed fallback constructor:

```rust
flags: Vec::new(),
```

`cli/src/tui/probe.rs:753` -- `flags: parsed.status_flags.clone()`
is unchanged (clones a Vec instead of a HashSet).

`cli/src/tui/app.rs:385-396` -- the `sample_ups_snapshot()` test
helper constructs `flags: std::collections::HashSet::new()`. Flip
to:

```rust
flags: Vec::new(),
```

Without this edit `just test-rust` fails to compile.

`cli/src/tui/browse/view.rs:418-420` -- mock fixture
`[UpsStatusFlag::Ol].into_iter().collect()` is unchanged (Vec
implements `FromIterator`).

### 5. Test updates

- `cli/src/parse/upsc.rs:104-154` -- assertions use `.contains()`,
  `.len()`, `.is_empty()`; all compile on `Vec`. **Add** one new
  test that pins the order contract directly:

  ```rust
  // Intent: parse_upsc preserves upsc's emission order in status_flags.
  // Why it exists: the human render and --json output both iterate
  // status_flags verbatim, so the parser's insertion order is the
  // script-facing contract. A regression would silently re-randomize
  // the array.
  // Scenario: a calibration with charging + replace-batt advisory,
  // which exercises four distinct flags in NUT's canonical emission order.
  #[test]
  fn parse_upsc_preserves_status_flag_order() {
      let out = parse_upsc("ups.status: CAL OL CHRG RB\n");
      assert_eq!(
          out.status_flags,
          vec![
              UpsStatusFlag::Cal,
              UpsStatusFlag::Ol,
              UpsStatusFlag::Chrg,
              UpsStatusFlag::Rb,
          ],
      );
  }
  ```

  Also add a malformed-input round-trip so the dedupe-on-push
  contract is pinned:

  ```rust
  // Intent: duplicates in ups.status collapse to a single flag, in
  // first-seen order.
  // Why: the Vec replaces a HashSet for ordering reasons; the set
  // semantics for membership must survive a driver that mistakenly
  // repeats a token.
  // Scenario: hand-crafted "ups.status: OL OB OL" (not produced by
  // any real NUT driver but is what the infallible parser contract
  // promises to handle).
  #[test]
  fn parse_upsc_dedupes_repeated_status_tokens() {
      let out = parse_upsc("ups.status: OL OB OL\n");
      assert_eq!(out.status_flags, vec![UpsStatusFlag::Ol, UpsStatusFlag::Ob]);
  }
  ```

- `cli/src/ups.rs` -- rewrite each `let mut s = HashSet::new();
  s.insert(...)` block as `vec![...]`. Hit list:
  `299-302`, `310-313`, `324-325`, `351-356`, `398-419`,
  `457`, `492-495`, `520-523`, `549-550`.

- `cli/src/ups.rs:308-314` -- rename
  `format_status_ob_lb_sorted` to
  `format_status_preserves_insertion_order`. Replace the single
  assertion with two:

  ```rust
  assert_eq!(format_status(&[UpsStatusFlag::Ob, UpsStatusFlag::Lb]), "OB LB");
  assert_eq!(format_status(&[UpsStatusFlag::Lb, UpsStatusFlag::Ob]), "LB OB");
  ```

  Update the test preamble: drop the "sorting" justification, state
  the new contract (rendered order = input order, matching `upsc`).

- `cli/src/ups.rs:387-447` --
  `json_output_status_flags_are_sorted`: rename to
  `json_output_status_flags_preserve_insertion_order`. Replace the
  alphabetical expected-Vec at 440-446 with the build-order Vec
  (the same `for flag in [Ol, Ob, Lb, Rb, Hb, ...]` sequence, in
  push order). Update the test preamble.

- `cli/src/tui/view/mod.rs:2109-2110` -- `flags_set` helper:

  ```rust
  fn flags_vec(tokens: &[UpsStatusFlag]) -> Vec<UpsStatusFlag> {
      tokens.to_vec()
  }
  ```

  Rename callers at `2128, 2146, 2156, 2179` accordingly.

- `cli/src/tui/view/mod.rs` -- add a behavioral test that pins the
  Data tab render's order independent of snapshots. Without it,
  `format_ups_flags` could silently regain a sort while the existing
  TUI snapshot tests (which use single-flag `OL`) keep passing:

  ```rust
  // Intent: format_ups_flags renders tokens in input order, no sort.
  // Why it exists: the Data tab is one of four UPS render surfaces;
  // all four iterate status_flags verbatim and a future
  // "let me sort for stability" edit on this site would diverge from
  // `upsc`, `braid ups status`, --json, and the browse render
  // without the snapshot suite catching it (every fixture-backed
  // snapshot is single-flag).
  // Scenario: critical state with on-battery and low-battery flags
  // in two opposite arrival orders.
  #[test]
  fn format_ups_flags_preserves_insertion_order() {
      assert_eq!(format_ups_flags(&[UpsStatusFlag::Ob, UpsStatusFlag::Lb]), "OB LB");
      assert_eq!(format_ups_flags(&[UpsStatusFlag::Lb, UpsStatusFlag::Ob]), "LB OB");
  }
  ```

- `cli/src/tui/browse/view.rs` -- the browse `ups_status_lines`
  path has no dedicated multi-flag test; the existing
  `snapshot_browse_nut_status` (line 705) only exercises single
  `OL`. Add a sibling snapshot that uses a two-flag mock to pin
  order at the browse render:

  ```rust
  // Intent: Browse > NUT > Status renders multi-flag ups.status in
  // upsc emission order. Pins the fourth sort site (the one missed
  // by the original three-site inventory) so a re-sort regression
  // shows up as a snapshot diff.
  // Scenario: dummy snapshot reporting "OB LB" (critical state).
  #[test]
  fn snapshot_browse_nut_status_multi_flag() {
      let mut model = model();
      model.ups_config = Some(Ups { name: "ups".into() });
      let mut snap = ups_snapshot();
      snap.flags = vec![
          crate::parse::types::UpsStatusFlag::Ob,
          crate::parse::types::UpsStatusFlag::Lb,
      ];
      model.ups = Some(snap);
      model.browse.select_next();
      snap!(buffer_to_string(&render(&model, 80, 14)));
  }
  ```

  Accept the resulting `.snap` file; it must show `Status   OB LB`.

### 6. Snapshot acceptance

`cli/src/snapshots/snapshot_human_lowbattery.snap:6` flips from
`Status: LB OB` to `Status: OB LB`. Accept with `cargo insta accept`
or hand-edit. The fixture
`cli/tests/fixtures/nixos-25.11/upsc/upsc-lowbattery.txt:31` already
has `ups.status: OB LB`, so the new snapshot matches `upsc`
byte-for-byte. The other three fixture-backed snapshots
(`snapshot_human_onbattery.snap`, `snapshot_human_online.snap`,
`snapshot_human_replace_battery.snap`) are single-flag or
already-aligned (`OL RB`) and do not change.

The new `snapshot_browse_nut_status_multi_flag.snap` (created by
the browse test added in Section 5) lands accepted on first run --
verify it shows `Status   OB LB`, matching the input
`vec![Ob, Lb]`.

The existing `snapshot_browse_nut_status.snap` (single-flag `OL`)
is unchanged.

### 7. Supersede the prior JSON-sort plan

`plans/impl/2026-05-14-ups-json-status-flags-ordering.md` documents
the lex-sort approach this pivot abandons. In the same commit that
lands the code change, add a `Superseded` header at the top of that
file with a pointer to this plan's promoted location (whatever
`plans/impl/<date>-...md` slug it gets). Not before (the supersession
isn't true until the code lands); not after (the intervening commit
would leave the doc contradicting reality).

### 8. NixOS VM tests

`tests/cli/braid-status-ups.py` only checks substring/membership
(`"Status: OL" in human`, `"OL" in parsed["status_flags"]`), no
multi-flag order pinning. `tests/module/ups-preflight-on-battery.py`
greps for `OB` membership only. No VM-test changes required; grep
`tests/` for `LB OB` before landing as a final safety check
(currently zero hits in test assertions; all hits are *inputs* to
`upsrw -s 'ups.status=OB LB'` or fixture state files).

## Critical files

- `cli/src/parse/upsc.rs` -- parser container + dedupe-on-push.
- `cli/src/parse/types.rs` -- field type, drop helper.
- `cli/src/ups.rs` -- render site + test fixtures.
- `cli/src/tui/view/mod.rs` -- render site, severity helper,
  `flags_set` test helper.
- `cli/src/tui/model.rs` -- `UpsSnapshot.flags` field.
- `cli/src/tui/probe.rs` -- query-failed fallback constructor.
- `cli/src/tui/app.rs` -- `sample_ups_snapshot()` test helper.
- `cli/src/tui/browse/view.rs` -- `ups_status_lines()` render site
  + new multi-flag snapshot test.
- `cli/src/snapshots/snapshot_human_lowbattery.snap` -- accept new
  render.
- `cli/src/tui/browse/snapshots/snapshot_browse_nut_status_multi_flag.snap`
  -- new file, accepted on first run.
- `plans/impl/2026-05-14-ups-json-status-flags-ordering.md` --
  mark Superseded.

## Reused helpers and patterns

- `UpsStatusFlag::as_token` (`cli/src/parse/types.rs:509-529`) --
  unchanged; both render sites and the per-flag serializer keep
  calling it.
- `UpsStatusFlag::from_token` (`cli/src/parse/upsc.rs:17-39`) --
  unchanged; the parser still routes tokens through it before push.
- `impl Serialize for UpsStatusFlag`
  (`cli/src/parse/types.rs:568-575`) -- unchanged; emits one
  `serialize_str(as_token())`. Only the array-level sort hook goes
  away.
- `UpscOutput::is_critical` / `is_on_battery`
  (`cli/src/parse/types.rs:553-565`) -- unchanged; `.iter().any()`
  and `.contains()` work on slices.

## Verification

1. `just test-rust` -- exercises every unit test touched above plus
   the new order-contract tests. Must pass.
2. `cargo insta accept` (or `INSTA_UPDATE=auto cargo test`) for the
   one snapshot delta at
   `cli/src/snapshots/snapshot_human_lowbattery.snap:6`. Verify the
   diff is `LB OB` -> `OB LB` and nothing else.
3. `just test-vm braid-status-ups` -- live dummy-ups canary. The
   test does not pin multi-flag order so it should pass unchanged;
   running it confirms the parser still round-trips against real
   `upsc` output.
4. `just test-vm ups-preflight-on-battery ups-lb-clean-shutdown
   ups-lb-during-remove ups-lb-during-replace ups-lb-during-balanced-add
   ups-lb-during-remove-missing` -- the matrix of UPS scenarios.
   None pin render order, but they do drive `OB LB` state via `upsrw`
   and check braid's reaction; running them confirms no preflight
   classification breakage.
5. Manual spot-check: in the same VM, run
   `diff <(upsc ups | grep '^ups\.status:') <(braid ups status |
   grep '^Status:' | sed 's/^Status: /ups.status: /')`. The two
   lines should now be byte-identical.
6. `grep -rn 'Status: LB OB\|HashSet<UpsStatusFlag>' cli/src tests`
   -- both patterns should return zero hits after the change. The
   `Status: ` prefix is intentional: the reverse-order test
   assertions in Section 5 (`format_status_preserves_insertion_order`
   and `format_ups_flags_preserves_insertion_order`) contain bare
   `"LB OB"` string literals as input-order coverage, so a bare-token
   grep would match them and fail even on a correct implementation.
   The prefixed form only matches stale rendered output.
7. `grep -rn 'flags.sort\|tokens.sort' cli/src/ups.rs
   cli/src/tui/ cli/src/parse/` -- zero hits; all four sort sites
   are gone.
