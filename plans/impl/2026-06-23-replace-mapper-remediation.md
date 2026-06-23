# Fix: interpolate the real mapper name in the execute-time backing-mismatch remediation

## Context

`ReplaceError::NewTargetMapperBackingMismatch` (`cli/src/replace.rs`) is the
execute-time gate that fires in the post-confirmation TOCTOU window when an
already-open ExistingLuks replace target's mapper turns out to be backed by a
different disk than configured. Its error message tells the operator to fix the
conflict by running:

```
... close the conflicting mapper with 'sudo cryptsetup close braid-<name>' and re-run.
```

The `braid-<name>` token is a **literal** -- the operator is handed a
non-runnable command and must reverse-engineer the actual mapper. Every sibling
variant that emits this same remediation sentence interpolates the real name
instead: `ProbeError::MapperBackingMismatch` / `MapperConflict` (`cli/src/probe.rs`)
and `LuksError::MapperBackingMismatch` / `MapperConflict` (`cli/src/luks.rs`) all
render `braid-{name}`. The `replace.rs` variant is the lone drifted copy.

The variant was born with the placeholder in commit `98f02752` and never
corrected. Root cause: the remediation sentence is copy-pasted across five
variants, and this copy lost its interpolation because the variant carries only
`by_id` / `expected_path` / `found_path` -- no name field -- even though the
constructing function `verify_existing_luks_open_mapper_target` holds both
`new_name: &DiskName` and `new_mapper: &MapperName` in scope.

**Outcome:** the execute-time message renders a runnable command
(`sudo cryptsetup close braid-disk3`), matching its plan-time siblings, guarded
by a test that asserts the rendered string.

**Scope (decided):** focused fix only. The broader 5-variant / byte-identical
probe.rs<->luks.rs duplication is a real but pre-existing smell across
deliberately-different framings and is **out of scope** -- the four sibling
variants already interpolate the real name correctly, and touching them risks
regressions for pure-DRY benefit. See "Out of scope" below.

## Change

Three edits, all in `cli/src/replace.rs`. The fix adds a `mapper: MapperName`
field (not a disk-name `String`) because:

- The variant frames identity by `by_id`, not disk name -- the only thing it
  needs a name for is the remediation command, which is precisely the mapper.
- `new_mapper: &MapperName` is already in scope at the single construction site.
- `MapperName`'s `Display` renders the full basename `braid-disk3`, so
  interpolating `{mapper}` yields `sudo cryptsetup close braid-disk3` -- output
  identical to the siblings, with no re-derivation of the `braid-` prefix (that
  convention lives once in `config::mapper_name`).

### 1. Variant declaration (`cli/src/replace.rs`, the `NewTargetMapperBackingMismatch` arm, ~lines 93-102)

Add the `mapper` field and swap the literal token for `{mapper}`:

```rust
#[error(
    "replace target '{by_id}' open mapper backing mismatch: mapper is backed by \
     '{found_path}', expected '{expected_path}' -- close the conflicting mapper \
     with 'sudo cryptsetup close {mapper}' and re-run."
)]
NewTargetMapperBackingMismatch {
    by_id: ByIdPath,
    mapper: MapperName,
    expected_path: String,
    found_path: String,
},
```

`MapperName` is already imported in `replace.rs` (used in function signatures)
and derives `Debug`. Optional polish: add a one-line `///` on the variant (its
sibling `NewTargetUuidMismatchAtOpen` has one) noting it carries the conflicting
mapper handle so the remediation command is runnable.

### 2. Construction site (`cli/src/replace.rs`, `verify_existing_luks_open_mapper_target` -> the `OwnershipError::BackingPathMismatch` arm, ~line 1051)

Pass the in-scope mapper through:

```rust
} => ReplaceError::NewTargetMapperBackingMismatch {
    by_id: new_by_id.clone(),
    mapper: new_mapper.clone(),
    expected_path,
    found_path,
},
```

This is the only construction site (confirmed by grep). The struct-literal will
fail to compile until the field is added, and any other struct-pattern match
without `..` would likewise be flagged by the compiler -- so the change is
compiler-guided.

### 3. Test (`cli/src/replace.rs`, `replace_existing_luks_open_mapper_backing_mismatch_aborts`, ~lines 8174-8185)

The test already constructs with `MapperName::from_basename("braid-disk3".into())`
but currently asserts only the struct fields -- it never renders the `Display`
string, so it cannot catch a format-string regression. Add the `Display`
assertion (the one that actually guards this bug) and extend the destructure:

```rust
let rendered = err.to_string();
assert!(
    rendered.contains("sudo cryptsetup close braid-disk3"),
    "remediation must name the resolved mapper, got: {rendered}"
);
match err {
    ReplaceError::NewTargetMapperBackingMismatch {
        by_id: err_by_id,
        mapper,
        expected_path,
        found_path,
    } => {
        assert_eq!(err_by_id, by_id);
        assert_eq!(mapper, MapperName::from_basename("braid-disk3".into()));
        assert_eq!(expected_path, "/dev/vdb");
        assert_eq!(found_path, "/dev/vdz");
    }
    other => panic!("expected NewTargetMapperBackingMismatch, got: {other:?}"),
}
```

Compute `rendered` before the `match` (the `to_string()` borrow ends before the
match moves `err`).

## Out of scope (deliberate)

- Centralizing the copy-pasted "close the conflicting mapper..." sentence into a
  shared helper or onto `OwnershipError`. `OwnershipError`'s `Display` is terse
  and does not carry this sentence; the five outer variants add it with
  divergent framing (capitalization, embedded `/dev/mapper/braid-{name}` paths,
  and `replace.rs`'s `by_id`-centered wording), so no single helper covers them
  cleanly. The probe.rs/luks.rs byte-identical pair is the stronger dedup
  candidate but already interpolates the real name correctly -- a separate
  refactor, not this Low finding. Coverage is also uneven: the `MapperConflict`
  siblings have exact rendered-string tests (`luks.rs` ~1265/1285, `probe.rs`
  ~696/716), but the `MapperBackingMismatch` siblings do not --
  `ProbeError::MapperBackingMismatch` has only a structured-field test
  (`probe.rs` ~1017) and `LuksError::MapperBackingMismatch` none -- so a safe
  dedup would first have to add display-string coverage there, reinforcing that
  it is a separate effort.

## Verification

1. Focused test (should fail before edit 1+3, pass after). The CLI crate's
   package name is `braid-cli`, not the `braid` binary:
   ```
   cargo test -p braid-cli --lib replace_existing_luks_open_mapper_backing_mismatch_aborts
   ```
2. Full Rust unit suite: `just test-rust`.
3. Lint: `just clippy` (= `cargo clippy --manifest-path cli/Cargo.toml --tests`).
4. Format: `cargo fmt --check` (no new Unicode introduced; the message stays
   ASCII, satisfying `scripts/docs/check-output-ascii.py`).
5. Targeted grep -- after the fix this returns zero hits. A broad
   `rg 'braid-<name>' cli/src/` would false-positive on the legitimate
   `discover.rs#NoMembersDiscovered` user-facing string, so scope it to the
   remediation clause:
   ```
   rg -n "sudo cryptsetup close braid-<name>|close the conflicting mapper.*braid-<name>" cli/src
   ```
