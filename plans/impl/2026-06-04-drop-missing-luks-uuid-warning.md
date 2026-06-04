# Plan: drop the unreachable `MissingLuksUuid` discover warning

## Context

`DiscoverWarning::MissingLuksUuid` in `cli/src/discover.rs` is unreachable
against real `cryptsetup luksDump` output, and the test that exercises it
(`discover_warns_when_uuid_line_missing`) models a scenario cryptsetup never
produces.

`LUKS2_hdr_dump` (`reference/cryptsetup/lib/luks2/luks2_json_metadata.c#LUKS2_hdr_dump`)
unconditionally emits the `UUID:` line, printing the literal sentinel
`(no UUID)` when the header UUID is empty:

```c
log_std(cd, "UUID:          \t%s\n", *hdr->uuid ? hdr->uuid : "(no UUID)");
```

So for a LUKS2 header:

- A present, garbage, or empty UUID always yields a `UUID:` line. An empty
  in-memory UUID field prints `(no UUID)`, which `LuksUuid::parse` rejects ->
  `ParseError::InvalidValue` -> `DiscoverWarning::InvalidLuksUuid { raw: "(no UUID)" }`.
- The only trigger for `MissingLuksUuid` is `ParseError::MissingField`, which
  fires only when *no* `UUID:` line exists at all -- something real cryptsetup
  never produces for a LUKS2 header. Discover reaches the UUID parse only after
  confirming exit 0 and a parseable `Version: 2`, which guarantees the dump went
  through `LUKS2_hdr_dump` and therefore carries the `UUID:` line.

The variant is dead, and the old test's "header zeroed mid-format leaving a
label but no UUID" scenario is wrong on two counts. First, real LUKS2 output
never omits the `UUID:` line (`LUKS2_hdr_dump` always emits it). Second, an
empty in-memory UUID field prints the `(no UUID)` sentinel that routes to
`InvalidLuksUuid` -- so a loadable LUKS2 header never reaches the missing-line
path the old test fed. (How `hdr->uuid` could come to be empty is not the
point and is not proven here: both the format path
(`reference/cryptsetup/lib/luks2/luks2_json_format.c#LUKS2_generate_hdr`) and
the set-UUID path
(`reference/cryptsetup/lib/luks2/luks2_json_metadata.c#LUKS2_hdr_uuid`) always
generate or validate a UUID. What is proven is only the dump-time print, and
that is the behavior braid must handle.)

The sibling field already shows the right pattern. A missing or malformed
`Version:` field folds into the generic `LuksDumpUnparseable` bucket (the
`parse_cryptsetup_luks_version` match arm in `discover_from_dir_inner`), pinned
by `discover_warns_on_unparseable_luksdump_output`. UUID is the lone luksDump
field with a dedicated missing-variant. This pivot makes UUID match Version.

**Outcome:** a smaller `DiscoverWarning` enum, a test suite that models what
cryptsetup actually emits, and consistent "missing required field" routing
across the luksDump body.

## Approach

Drop `MissingLuksUuid`; route the (drift-only) absent-`UUID:`-line case through
the existing `LuksDumpUnparseable` catch-all, exactly as the absent-`Version:`
case already is. Keep `InvalidLuksUuid` -- it is the reachable empty/garbage
path, including the real `(no UUID)` sentinel. Relocate the original test's
behavioral intent ("a disk with no usable UUID is skipped and warned, never
silently admitted to membership") onto the two cases that can actually occur.

### Edits -- `cli/src/discover.rs`

1. **Remove the `MissingLuksUuid` variant** from `enum DiscoverWarning` (its
   `/// Discovery read ... no UUID: line ...` doc comment plus the variant).
2. **Remove its `Display` match arm** (the
   `"skipping {path}: luksDump output missing UUID"` arm).
3. **Remove the `Err(ParseError::MissingField { .. })` arm** in the
   `parse_cryptsetup_luks_uuid_from_dump` match inside `discover_from_dir_inner`.
   The existing catch-all `Err(e) => LuksDumpUnparseable { detail: e.to_string() }`
   then handles it, mirroring the Version arm. Keep the `InvalidValue` arm -- it
   extracts `raw` for `InvalidLuksUuid`.

   Post-change, an absent `UUID:` line renders as:
   `skipping <path>: luksDump output unparseable -- missing field ` + "`UUID`" + ` in output of ` + "`cryptsetup`"
   (the `ParseError::MissingField` Display from `cli/src/parse/mod.rs`, matching
   the Version precedent).

### Edits -- tests in `cli/src/discover.rs`

4. **Recast `discover_warns_when_uuid_line_missing`** (rename to e.g.
   `discover_treats_absent_uuid_line_as_unparseable`):
   - Keep feeding `luksdump_body("braid-baddisk", None)` (omits the `UUID:` line).
   - Assert `DiscoverWarning::LuksDumpUnparseable` with `detail.contains("UUID")`,
     and the disk absent from members.
   - Rewrite the preamble: the truthful scenario is **parser drift** -- a future
     cryptsetup that renames or removes the `UUID:` line. Note that real LUKS2
     output always carries the line (`reference/cryptsetup/lib/luks2/luks2_json_metadata.c#LUKS2_hdr_dump`),
     so this guards against silently admitting an identity-less disk on upstream
     format drift, not against an empty header.

5. **Add `discover_warns_when_header_uuid_is_no_uuid_sentinel`** -- the true,
   reachable empty-UUID-field case:
   - Feed `luksdump_body("braid-baddisk", Some("UUID:\t(no UUID)"))`.
   - Assert `DiscoverWarning::InvalidLuksUuid { raw: "(no UUID)", .. }`, disk absent.
   - Write the preamble to the *observable* contract, not a causal story:
     cryptsetup successfully loads a LUKS2 header whose binary UUID field is
     empty, so `luksDump` prints the `(no UUID)` sentinel
     (`LUKS2_hdr_dump`: `*hdr->uuid ? hdr->uuid : "(no UUID)"`); braid must
     surface that as `InvalidLuksUuid` and skip the disk, never admit it.
     Do **not** claim a "zeroed mid-format" header reaches this path -- both
     the format path (`LUKS2_generate_hdr` in `luks2_json_format.c`) and the
     set-UUID path (`LUKS2_hdr_uuid` in `luks2_json_metadata.c`) always
     generate or validate a UUID, so how the field came to be empty is
     unproven; only the dump-time `(no UUID)` print is. This replaces the
     false scenario the deleted test asserted.

Both tests assert behavioral, structure-insensitive contracts (member absent +
warning present + correct variant + detail substring).

### Edits -- `cli/src/parse/cryptsetup_luks_uuid.rs`

6. **Fix the stale comment** on `luks_uuid_from_dump_returns_missing_field_when_absent`
   (the `// Why: discover maps the missing-field outcome to DiscoverWarning::MissingLuksUuid ...`
   block): update to "discover folds the missing-field outcome into
   `LuksDumpUnparseable` (the parser-drift bucket, matching the missing-`Version:`
   path)." The parser test body stays unchanged -- `MissingField` vs
   `InvalidValue` remains a valid parser contract.

7. **(Recommended) Add parser contract test** `luks_uuid_from_dump_rejects_no_uuid_sentinel`:
   feed `UUID:\t(no UUID)`, assert `ParseError::InvalidValue { raw: "(no UUID)", .. }`.
   Pins the exact real-cryptsetup sentinel at the producing parser; no existing
   test feeds it (the current `InvalidValue` tests use `not-a-uuid` and
   `not (a uuid)`).

### Do not touch

- **Parser error taxonomy.** Keep `parse_cryptsetup_luks_uuid_from_dump`
  returning `MissingField` for the no-line case and `InvalidValue` for bad
  values. `InvalidValue` is still consumed for `InvalidLuksUuid`; `MissingField`
  stays a meaningful, tested parser contract even though discover now buckets it
  generically.
- **`InvalidLuksUuid` variant.** Reachable; keep as-is.
- **`plans/impl/...` references** to `MissingLuksUuid` (in
  `2026-05-12-luks-uuid-as-identity/plan.md` and `2026-05-18-discover-read-dir-errors.md`).
  These are frozen point-in-time records, not live contracts (per AGENTS.md
  "Decision Doc References"). Leave them.
- No `docs/` or `README.md` change -- neither references the variant or its
  message.

## Verification

- `just test-rust` -- runs `cargo test` for `braid-cli`. Primary gate; this is a
  pure Rust unit-test change.
  - Compilation confirms the `Display` exhaustive match was updated (the only
    compile-breaking edit).
  - Touched/adjacent tests that must stay green: the recast and new discover
    tests above, plus `discover_warns_when_uuid_unparseable`,
    `discover_warns_on_unparseable_luksdump_output`, and the
    `luks_uuid_from_dump_*` parser tests.
- No fixture refresh (not a parser-critical tool-version change) and no VM tests
  (no module / systemd / lifecycle / mount change).
