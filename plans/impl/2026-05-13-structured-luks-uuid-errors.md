# Plan: structured `raw`/`detail` across the LUKS UUID dump-parser boundary

## Context

`parse_cryptsetup_luks_uuid_from_dump` rejects a malformed `UUID:` value
by stuffing the underlying `LuksUuidParseError`'s two structured fields
into a single formatted string:

```rust
// cli/src/parse/cryptsetup_luks_uuid.rs:69-73
LuksUuid::parse(&raw_value).map_err(|e| ParseError::UnexpectedValue {
    cmd: raw.cmd.clone(),
    field: "UUID".into(),
    value: format!("{} ({})", e.raw, e.detail),
})
```

`discover` then reverses that exact format with `value.find(" (")`:

```rust
// cli/src/discover.rs:382-399
Err(ParseError::UnexpectedValue { value, .. }) => {
    let (raw, detail) = match value.find(" (") {
        Some(idx) => (
            value[..idx].to_owned(),
            value[idx + 2..].trim_end_matches(')').to_owned(),
        ),
        None => (value.clone(), String::new()),
    };
    warnings.push(DiscoverWarning::InvalidLuksUuid { path: ..., raw, detail });
    continue;
}
```

The round-trip is fragile: if `e.raw` ever contains the literal
delimiter `" ("`, the reverse split lands inside `raw` and corrupts the
warning. Example: `e.raw = "not (a uuid)"` formats to
`"not (a uuid) (<detail>)"`; `value.find(" (")` matches at index 3
(between `not` and `(a uuid)`), so the warning shows `raw = "not"` and
`detail = "a uuid) (<detail>"`.

`cryptsetup` already has a real emit path that puts parens on the
`UUID:` line -- `reference/cryptsetup/lib/luks2/luks2_json_metadata.c:2202`
falls back to the literal `"(no UUID)"` when the header UUID is empty.
That specific text happens to round-trip correctly by coincidence (no
`" ("` substring; the first match is still the field join). But any
paren-bearing malformed value with a `" ("` substring -- a corrupted
header, a future cryptsetup format change, an operator-pasted test
value -- breaks the split. There is no test that pins the structured
fields against input containing the delimiter.

The shared `ParseError::UnexpectedValue` variant has two other call
sites (`cli/src/parse/btrfs_filesystem_df.rs:65`,
`cli/src/parse/cryptsetup_luks_version.rs:38`) that treat `value` as a
plain bad-value string. The UUID-from-dump parser is the only outlier
abusing `value` to carry a sub-structured payload.

Goal: end the format/split round-trip so `raw` and `detail` flow as
typed fields, and pin the contract with a paren-bearing test value.

## Approach

Add a purely additive sibling variant to `ParseError` whose payload is
the structured (raw, detail) pair that the LUKS UUID dump parser already
has. Switch the UUID parser to it; switch the discover consumer to match
on it directly. Leave the existing `UnexpectedValue` users untouched --
their semantics (value-rejected with no underlying parser detail) stay
correct.

### New variant

In `cli/src/parse/mod.rs` (alongside `UnexpectedValue` at lines 60-66):

```rust
/// Value at `field` was rejected by a typed sub-parser. `raw` is the
/// offending input verbatim; `detail` is the sub-parser's reason. Kept
/// separate so downstream consumers (e.g. discover warnings) don't have
/// to reverse a formatted string to recover both halves.
#[error("invalid value `{raw}` for field `{field}` in output of `{cmd}`: {detail}")]
InvalidValue {
    cmd: String,
    field: String,
    raw: String,
    detail: String,
},
```

Naming rationale: `UnexpectedValue` keeps its meaning ("value not in the
expected set", e.g. unknown btrfs `bg_type`, unparseable `Version:`).
`InvalidValue` is the typed-sub-parser-rejection case where there is an
explanation worth carrying. Both can coexist; future typed parsers can
opt into `InvalidValue` without touching the simpler users.

### Parser switch

`cli/src/parse/cryptsetup_luks_uuid.rs:69-73` -- replace the
`format!(...)` with structured fields:

```rust
LuksUuid::parse(&raw_value).map_err(|e| ParseError::InvalidValue {
    cmd: raw.cmd.clone(),
    field: "UUID".into(),
    raw: e.raw,
    detail: e.detail,
})
```

### Consumer switch

`cli/src/discover.rs:382-399` -- drop the `value.find(" (")` block and
match the new variant directly:

```rust
Err(ParseError::InvalidValue { raw, detail, .. }) => {
    warnings.push(DiscoverWarning::InvalidLuksUuid {
        path: path_str.clone(),
        raw,
        detail,
    });
    continue;
}
```

`DiscoverWarning::InvalidLuksUuid` (`cli/src/discover.rs:80-85`) and its
`Display` impl (`cli/src/discover.rs:119-122`) stay unchanged --
`{detail}` is already a plain field and the user-facing format is
preserved.

## Critical files

- `cli/src/parse/mod.rs` -- add the `InvalidValue` variant (`enum ParseError` at lines 43-66).
- `cli/src/parse/cryptsetup_luks_uuid.rs` -- swap the error construction at lines 69-73; update the test at lines 215-231 to match on `InvalidValue` and assert on `raw`/`detail` directly; add a new test that feeds `"not (a uuid)"` as the `UUID:` value (the value contains the literal `" ("` delimiter the old code split on) and pins `raw == "not (a uuid)"` plus a non-empty `detail`.
- `cli/src/discover.rs` -- replace the match arm at lines 382-399 with a direct destructure; add a new test alongside `discover_warns_when_uuid_unparseable` (`cli/src/discover.rs:1334-1374`) that uses `"not (a uuid)"` as the raw UUID and asserts the discovered warning carries `raw == "not (a uuid)"` (regression pin against the old split corruption -- the old code would have produced `raw == "not"`).

Nothing else needs to move:

- `cli/src/parse/cryptsetup_luks_version.rs:38` and `cli/src/parse/btrfs_filesystem_df.rs:65` keep using `UnexpectedValue` -- their failure modes (unparseable u32, unknown enum string) have no typed sub-parser detail to carry.
- Public re-exports in `cli/src/parse/mod.rs:84-87` and downstream `#[from]` impls (`ack.rs:246`, `add.rs:85`, `replace.rs:125`) are unaffected -- they propagate `ParseError` opaquely.

## Reused types and helpers

- `LuksUuidParseError { raw, detail }` (`cli/src/types.rs:20-29`) is already the structured source -- the plan threads its two fields through without re-deriving them.
- `DiscoverWarning::InvalidLuksUuid { raw, detail }` (`cli/src/discover.rs:80-85`) is the existing warning shape and remains the only consumer surface.

## Test additions

Two new tests, both small and behavioral (they exercise the parser/
consumer contract, not the internal representation):

1. `cli/src/parse/cryptsetup_luks_uuid.rs` -- new test next to `luks_uuid_from_dump_returns_invalid_value_when_unparseable`:

   ```rust
   // Intent: a UUID: line whose value contains the literal " ("
   //   substring yields structured raw/detail fields with no
   //   string-round-trip corruption.
   // Why it exists: an earlier implementation packed raw+detail into a
   //   single formatted string ("<raw> (<detail>)") and discover
   //   reverse-split it on " ("; any raw containing " (" silently
   //   truncated at the first match.
   // Scenario: a corrupted or hand-edited LUKS2 header dump line of the
   //   form "UUID:          \tnot (a uuid)\n" -- the " (" between
   //   "not" and "(a uuid)" is exactly the delimiter the old split
   //   matched first.
   #[test]
   fn luks_uuid_from_dump_preserves_delimiter_bearing_raw() {
       let raw = RawCommandOutput {
           cmd: "cryptsetup luksDump".into(),
           stdout: "LUKS header information\nVersion:       \t2\nUUID:          \tnot (a uuid)\n".into(),
           stderr: String::new(),
           exit_status: 0,
       };
       let err = parse_cryptsetup_luks_uuid_from_dump(&raw).unwrap_err();
       match err {
           ParseError::InvalidValue { field, raw, detail, .. } => {
               assert_eq!(field, "UUID");
               assert_eq!(raw, "not (a uuid)");
               assert!(!detail.is_empty(), "detail must carry uuid-crate reason");
           }
           other => panic!("expected InvalidValue UUID, got {other:?}"),
       }
   }
   ```

2. `cli/src/discover.rs` -- new test next to `discover_warns_when_uuid_unparseable`:

   ```rust
   // Intent: discover surfaces an invalid UUID value containing the
   //   literal " (" substring with raw and detail intact in
   //   DiscoverWarning::InvalidLuksUuid.
   // Why it exists: an earlier implementation reverse-split a formatted
   //   "<raw> (<detail>)" string on " (" inside discover; if raw itself
   //   contained " (", the warning showed a truncated raw (e.g. "not"
   //   for input "not (a uuid)") and a malformed detail.
   // Scenario: a corrupted LUKS2 header where the UUID: line reads
   //   "not (a uuid)" -- the " (" between "not" and "(a uuid)" is the
   //   exact delimiter the old split matched first.
   #[test]
   fn discover_warns_when_uuid_value_contains_split_delimiter() {
       // ... build runner with luksdump_body emitting "UUID:\tnot (a uuid)"
       // ... assert DiscoverWarning::InvalidLuksUuid { raw: "not (a uuid)", detail: non-empty, .. }
       // ... assert warning.to_string() contains "not (a uuid)" exactly once
       // ... assert raw != "not" (regression pin against the old split)
   }
   ```

The existing tests at `cli/src/parse/cryptsetup_luks_uuid.rs:215-231`
and `cli/src/discover.rs:1334-1374` need their pattern-matches updated
from `UnexpectedValue { value, .. }` to `InvalidValue { raw, detail, .. }`;
the assertions stay equivalent.

## Verification

End-to-end check after applying the changes:

1. `just test-rust` -- exercises the updated parser unit tests
   (`luks_uuid_from_dump_returns_invalid_value_when_unparseable`,
   the new `luks_uuid_from_dump_preserves_delimiter_bearing_raw`) and
   the discover unit tests (`discover_warns_when_uuid_unparseable`,
   the new `discover_warns_when_uuid_value_contains_split_delimiter`).
2. `just test-vm` -- guards against regressing the discover VM tests
   that flow through the `LuksDumpUnparseable` / `InvalidLuksUuid`
   warning paths.
3. `cargo clippy --all-targets` -- catches any leftover field-name
   references (`value` vs `raw`) in test matches.

No fixture refresh is required: this plan does not touch any parser
that consumes pinned tool output (`btrfs-progs`, `cryptsetup`,
`util-linux`, `nut` schemas are unchanged) and adds no new tool-version
coupling.
