# Plan: cheap parser-layer guard for empty-value `ups.status`

## Context

`parse_upsc` returns an empty `status_flags` set for two distinct inputs:
an **absent** `ups.status` key (the match arm never runs) and a
**present-but-empty** `ups.status:` value (the arm runs and
`value.split_ascii_whitespace()` yields zero tokens). Two documented
sentinels hinge on the empty result:

- the `ups_status_empty` JSON warning (`cli/src/ups.rs#JsonSuccessReport::new`,
  `status_flags.is_empty().then_some(...)`), and
- the human `(unknown -- ups.status missing)` line (`cli/src/ups.rs#format_status`).

The empty-**value** path is guarded only implicitly: there is no explicit
"if value empty, skip" -- correctness rides entirely on
`split_ascii_whitespace` returning nothing for `""`. A regression to a
literal split (`split(' ')`) would yield one `""` token -> `Unknown("")`,
populating `status_flags`, and silently break both sentinels. Confirmed
with `rustc`: `"".split_ascii_whitespace().count() == 0` but
`"".split(' ').count() == 1`.

No Rust test exercises that path. Every empty-case unit test either omits
the `ups.status` line entirely (`empty_status_produces_no_flags`, an
absent-key input) or hand-builds `UpscOutput { status_flags: Vec::new() }`
without calling the parser (`format_status_empty_is_unknown`,
`format_human_empty_status_renders_sentinel`,
`json_output_with_empty_status_has_warning_and_body`). The one test that
drives the parser through a `ups.status` line uses a non-empty value
(`json_all_unknown_status_has_no_warning`). Only the
`tests/cli/braid-status-ups` VM canary (fixture `emptyups.dev` ends with
`ups.status:`; assertion `status_flags == []`) covers it -- a real guard,
but slow and far from the parser.

Compounding it: the existing `empty_status_produces_no_flags` Intent
comment claims coverage for "absent **or empty** ups.status" while its
body tests only the absent case -- the comment over-claims.

**Outcome:** convert the VM-only, implicit invariant into a cheap unit
guard, and make the existing test's comment honest.

## Approach

Test-only change in one file: `cli/src/parse/upsc.rs` (the `#[cfg(test)]`
module). No production code changes, no fixture changes. The parser
already behaves correctly; this pins the behavior.

### 1. Narrow + rename the existing absent-key test

Rename `empty_status_produces_no_flags` -> `absent_status_produces_no_flags`
and narrow its Intent line from "absent or empty ups.status" to "absent
ups.status". Its `Why` ("emits no ups.status line") and `Scenario`
("stub file before the first status write") are already absent-specific
and stay as-is. Body unchanged. The rename removes the name collision with
the new test below (`empty_status_*` vs `empty_status_value_*` would read
confusingly close).

### 2. Add a dedicated empty-value test

New test driving the real parser, with a full Intent / Why / Scenario
preamble per AGENTS.md, naming the exact regression and the VM-canary
linkage:

```rust
// Intent: an explicit-but-empty `ups.status:` value yields zero flags --
// the same empty set as an absent key, but via a different code path
// (the match arm runs; the split loop iterates zero times).
// Why it exists: this empty-set result rides entirely on
// `split_ascii_whitespace` yielding no tokens for "" -- there is no
// explicit empty guard. A swap to `split(' ')` would emit a stray
// `Unknown("")`, silently breaking the `ups_status_empty` JSON warning
// and the `(unknown -- ups.status missing)` human sentinel. Until now
// only the `emptyups.dev` VM canary (tests/cli/braid-status-ups) covered
// this path; this is its cheap unit mirror.
// Scenario: a driver publishes ups.status with no tokens -- the dummy-ups
// `emptyups.dev` fixture sets an explicit empty status line, since the
// driver would otherwise default a missing status to OL.
#[test]
fn empty_status_value_produces_no_flags() {
    let out = parse_upsc("battery.charge: 55\nups.status:\n");
    assert!(out.status_flags.is_empty());
    // charge still parses -> the empty status line was consumed, not that
    // the whole parse degraded to empty.
    assert_eq!(out.battery.charge_pct, Some(55));
    // The empty status line routes to the typed arm, not `extra`.
    assert!(out.extra.get("ups.status").is_none());
}
```

Three complementary assertions, each guarding a distinct failure:
- `status_flags.is_empty()` -- the core invariant the `split(' ')`
  regression would break.
- `battery.charge_pct == Some(55)` -- distinguishes "empty value -> no
  flags" from "parse bailed entirely" (which would also be empty). Mirrors
  the input shape the `emptyups.dev` fixture uses (`battery.charge: 55`).
- `extra.get("ups.status").is_none()` -- pins that the key is consumed by
  the typed `ups.status` arm, not dumped into `extra` (precedent:
  `parses_rich_model_fields`' "No stray extras" assertions).

### Reused code (no new helpers)

- `parse_upsc` (`cli/src/parse/upsc.rs#parse_upsc`) -- the unit under test.
- `UpscOutput` fields `status_flags`, `battery.charge_pct`, `extra`
  (`cli/src/parse/types.rs`) -- already the assertion surface used by
  sibling tests.

No downstream sentinel assertions are duplicated here: the empty-set ->
warning/human-sentinel chain is already pinned by
`json_output_with_empty_status_has_warning_and_body`,
`format_status_empty_is_unknown`, and
`format_human_empty_status_renders_sentinel`. This test closes only the
parser-input gap that feeds them.

## Verification

1. **Format:** `cargo fmt` from `cli/` (rustfmt line-wrapping is enforced;
   see commit `b0251cb2`).
2. **Green:** `just test-rust` (runs `cargo test --lib ...`, which covers
   `cli/src/parse/upsc.rs` via `pub mod parse;` in `cli/src/lib.rs`).
   Targeted: `cargo test --lib empty_status_value_produces_no_flags` and
   `cargo test --lib absent_status_produces_no_flags` from `cli/`.
3. **Red-for-the-right-reason (braid TDD ethos, AGENTS.md):** temporarily
   change `value.split_ascii_whitespace()` to `value.split(' ')` in the
   `"ups.status"` arm and confirm `empty_status_value_produces_no_flags`
   fails on the `status_flags.is_empty()` assertion (a stray `Unknown("")`
   appears) while the non-empty status tests still pass. Revert.
4. No VM run required -- this is a pure parser unit test. The existing
   `braid-status-ups` VM canary remains the end-to-end mirror.

## Out of scope

- No change to `parse_upsc` itself -- behavior is already correct.
- No change to `emptyups.dev` / the VM test -- the canary stays as the
  live-tool mirror.
- No sibling pattern to unify: `ups.status` is the only field parsed via a
  whitespace split; all other fields use explicit empty guards
  (`some_non_empty`) or `.parse()` / `parse_pct` (empty -> `None`).
