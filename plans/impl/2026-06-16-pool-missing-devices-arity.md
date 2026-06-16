# Decide `pool_missing_devices` arity exactly once

## Context

`check_pool_missing_devices` in `cli/src/doctor.rs` builds its degraded-pool
warning by making the *same* singular/plural decision in four independent
places:

- `cross_check` -- a `match pool.missing_devids.as_slice()` (`[devid]` vs `_`),
  `doctor.rs` ~:811-817
- `cross_check_target` -- an `if n == 1`, ~:818-822
- two inline `if n == 1 { "" } else { "s" }` pluralizers for `device{}` and
  `devid{}`, ~:830-831

All four switch on the same arity, but nothing ties them together, so a future
edit can leave them disagreeing (e.g. a plural devid list beside a singular
"Use the listed ID."). A code-review finding flagged this. (The finding also
claimed the `devids` vector is dead -- it is not; it is consumed by
`devids.join(", ")` at ~:832 -- but the underlying cohesion concern is real.)

This change makes the arity decision exactly once, so a mixed singular/plural
message becomes structurally unrepresentable. It is behavior-preserving: the
same strings are emitted for each arity; only the branching is unified.

**Scope note (deliberately excluded).** The `if n == 1 { "" } else { "s" }`
idiom recurs at ~12 sites CLI-wide (plus 8 whole-word `"disk"/"disks"`,
`"is"/"are"` sites), with no shared helper. That dedup is real but *orthogonal*
to this finding -- a `plural_s` helper would not unify `cross_check` /
`cross_check_target` with the pluralizers, and a suffix-only helper would create
a second idiom alongside the whole-word sites. If pursued, it belongs in its own
focused commit that handles both forms (`plural_word(n, one, many)` + a thin
`plural_s`), mirroring the existing `refactor(cli): reuse command detail suffix
helper`. Do not bundle it here.

## Change

### 1. Collapse the warn arm to one arity match

`cli/src/doctor.rs`, `check_pool_missing_devices`, the `Ok(pool) =>` arm
(~:807-835). Replace the four independent switches with a single
`match pool.missing_devids.as_slice()` that yields every arity-dependent
fragment together, and reuse one `suffix` binding for both pluralized nouns:

```rust
Ok(pool) => {
    let n = pool.missing_devids.len();
    let (suffix, cross_check, cross_check_target) = match pool.missing_devids.as_slice() {
        [devid] => (
            "",
            format!(
                "Optional cross-check: `{}`.",
                repair_hint::missing_replace_command_with_devid(None, *devid)
            ),
            "Use the listed ID.",
        ),
        _ => (
            "s",
            repair_hint::optional_missing_id_cross_check_phrase(),
            "Use one of the listed IDs.",
        ),
    };
    let repair_command = repair_hint::missing_replace_command(None);
    let devids = pool
        .missing_devids
        .iter()
        .map(|d| d.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    CheckResult::warn(
        "pool_missing_devices",
        format!(
            "pool has {n} missing device{suffix} (devid{suffix}: {devids}); replace with: \
             `{repair_command}`; {cross_check} {cross_check_target} \
             Use `braid status` to see the missing disk's name",
        ),
    )
}
```

Why this is the right shape:

- The single `suffix` (`""` / `"s"`) feeds **both** `device{suffix}` and
  `devid{suffix}` -- the two inline pluralizers (the fragments the finding
  under-counted) collapse into one binding decided in the same arm.
- `devids` (the join) is arity-agnostic -- a 1-element join is just the element
  -- so it stays unconditional; no need to branch it (the finding suggested
  threading it through the match; that is unnecessary).
- The `_` arm means "2 or more": the empty case is already handled by the
  sibling `Ok(pool) if pool.missing_devids.is_empty()` arm at ~:804.
- No new helpers: reuses `repair_hint::missing_replace_command`,
  `missing_replace_command_with_devid`, and
  `optional_missing_id_cross_check_phrase` (`cli/src/repair_hint.rs`), already
  imported and used here.

### 2. Pin the singular arm

`cli/src/doctor.rs`, `pool_missing_devices_warns_with_replace_recommendation`
(~:5506, the `n == 1` test). Today this test only checks loose substrings
(`contains("missing device")`, `contains("devid")`), so the `[devid]` arm's
exact output is pinned by **no** test -- a future regression there would go
uncaught. The plural arm already has full pinning in
`pool_missing_devices_plural_warns_with_single_replace_command` (~:5552); give
the singular arm parity. The test's missing devid is `Devid::new(2)`, so add:

- `assert!(check.message.contains("pool has 1 missing device (devid: 2)"))`
  -- pins the singular suffix on **both** nouns (current `contains("devid")`
  also matches `"devids"`).
- `assert!(check.message.contains("Use the listed ID."))` -- currently unpinned.
- `assert!(check.message.contains("Optional cross-check: `braid replace --old <missing-name> --new <new-name>=/dev/disk/by-id/<...> --missing-id 2`."))`
  -- pins the concrete single-devid cross-check. (If preferred for brevity:
  `contains("Optional cross-check:")` + `contains("--missing-id 2")`.)

These are behavioral and structure-insensitive (assert on rendered output
substrings, not internal structure). They do not conflict with the existing
`!contains("braid replace --missing-id")` guard, because the concrete form has
`--old ... --new ...` between `braid replace` and `--missing-id`.

## Files

- `cli/src/doctor.rs` -- `check_pool_missing_devices` warn arm (collapse) and
  `pool_missing_devices_warns_with_replace_recommendation` test (strengthen).
  No other files.

## Reuse

- `repair_hint::missing_replace_command`, `missing_replace_command_with_devid`,
  `optional_missing_id_cross_check_phrase` -- `cli/src/repair_hint.rs`. All
  already used in this arm; the change consolidates their call sites and adds
  nothing new.

## Verification

- `just test-rust` -- the two existing arity tests plus the strengthened
  singular assertions must pass; behavior is unchanged. (Scoped run during
  iteration: `cargo test -p <cli-crate> pool_missing_devices`.)
- Behavior-preservation check (string equality against current output):
  - `n == 1` -> `"pool has 1 missing device (devid: 2); replace with: `braid replace --old <missing-name> --new <new-name>=/dev/disk/by-id/<...>`; Optional cross-check: `braid replace --old <missing-name> --new <new-name>=/dev/disk/by-id/<...> --missing-id 2`. Use the listed ID. Use `braid status` to see the missing disk's name"`
  - `n == 2` -> matches `pool_missing_devices_plural_warns_with_single_replace_command`:
    `"pool has 2 missing devices (devids: 2, 3) ... Optionally add `--missing-id <devid>` as a cross-check. Use one of the listed IDs. ..."`
- No fixture refresh (no `nixpkgs`/parser change). No docs change: the doctor
  table parity guard (`scripts/docs/check-doctor-table-parity.py`,
  `docs/commands/doctor.md`) guards the check **name**, which is unchanged; the
  human label `"missing devs"` (`doctor.rs` ~:1795) is unchanged.
- Out of scope and unaffected: `tests/cli/braid-add-warnings.py` pins
  single-form text emitted by `add.rs`/`preview.rs`, not this `doctor.rs` arm.
