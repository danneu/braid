# Plan: pin the fail-closed skip for null-backing and inactive mapper candidates

## Context

`classify_candidate_mapper` (`cli/src/lock.rs#classify_candidate_mapper`, ~lines 221-265)
issues `cryptsetup status` + `luksUUID` to prove a scanned `braid-*` mapper's identity
by backing LUKS UUID before lock teardown closes it. Two of its exit arms are
**fail-closed skips** -- they return `Err(...)` so the caller
(`push_uuid_classified_candidate`, ~357-383) warns, skips, and marks cleanup
`IncompleteUnclassified`, leaving the mapper open because its identity is unprovable:

- `CryptsetupStatusOutput::Active { backing: BackingDevice::Null }` (~238-245) -- cryptsetup
  reports `device: (null)`; backing is gone so no UUID can be read.
- `CryptsetupStatusOutput::Inactive` (~232-237) -- the dm slot was torn down between the
  `/dev/mapper` scan and the status call.

**Neither arm is exercised by any test.** The only use of the `cryptsetup_status_active_null`
fixture in `lock.rs` (line 4865, inside `full_arm_pass2_duplicate_devid_skips_and_warns_with_cleanup_uncertain`)
is a *negative* assertion proving Pass 3 never re-probes a skipped mapper
(`status_probe_count == 0`) -- it never enters the Null arm. `lock.rs` has no inactive
fixture at all.

A regression that demoted either outcome to **orphan-by-name** (and closed the mapper) would
pass every existing test -- a fail-open in the destructive teardown path. This plan pins both
arms. (The originating finding named only the Null arm and wrongly assumed Inactive was
already covered; the Inactive arm carries the identical regression risk, so the ideal fix
covers both at once -- dissolving the whole "status-variant `Err` arm silently demoted to
orphan-by-name" class rather than one instance.)

This is additive test-only work; no production behavior changes.

## Approach

Add two standalone `#[test]` functions to the `lock.rs` test module, plus one small fixture
helper. Drive `build_close_sets_uuid_scanned_fallback` directly (the unmounted fallback
builder, ~1072) with a single scanned `braid-*` candidate -- matching the existing
`uuid_scanned_fallback_*` test family (`uuid_scanned_fallback_preserves_member_then_orphan_close_order`
~4886, `uuid_scanned_fallback_malformed_mapper_with_uuid_is_orphan` ~4950). This isolates the
classification decision at the tightest failure locus; end-to-end preview rendering of the skip
is already covered generically by `unverified_fallback_candidate_is_warned_and_skipped` (~3153),
so it is not re-tested per arm.

Each test asserts the four behaviors the skip contract guarantees, plus one **arm-distinct**
substring so the test actually exercises (and pins) its specific arm rather than collapsing into
the already-covered generic status-`CmdError` skip:

1. `close_set.is_empty()` and `member_summaries(&close_set).is_empty()` and
   `orphan_summaries(&close_set).is_empty()` -- the candidate is skipped, **not** demoted to
   member or orphan. (Directly guards the "treated as orphan-by-name and closed" regression.)
2. `acc.cleanup.is_uncertain()` -- the unprovable skip marks cleanup uncertain.
3. A `PreviewNote::Warn` whose body contains `skipping mapper braid-<x>` **and** the arm's
   distinct detail (`reports null` for Null; `mapper is inactive` for Inactive). The distinct
   substring is what the finding flagged as "currently unreachable by any test."
4. No `CryptsetupLuksUuid` request was issued (`runner.requests()`), pinning that both arms
   short-circuit *before* attempting a UUID read on a null/absent backing.

### Files to modify

- `cli/src/lock.rs` (test module only): add the `cryptsetup_status_inactive` fixture next to
  `cryptsetup_status_active_null` (~1934), and add the two tests in the
  `uuid_scanned_fallback_*` neighborhood (~4886-4969).

### Fixture helper

The shape matches **real cryptsetup output**, verified against two authorities:

- Upstream source `reference/cryptsetup/src/cryptsetup.c#action_status`: the `CRYPT_INACTIVE`
  case emits `"%s/%s is inactive.\n"` via `log_std` (~line 951-953). `log_std` is
  `crypt_logf(NULL, CRYPT_LOG_NORMAL, ...)` (`cryptsetup.h`), i.e. it writes to **stdout**, not
  stderr.
- Captured golden fixtures: `cli/tests/fixtures/{nixos-26.05,nixos-unstable}/cryptsetup-status-inactive.stdout`
  holds `/dev/mapper/braid-vdb is inactive.` while the sibling `.stderr` capture is **empty**.

So the line goes on **stdout** with empty stderr (exit 4 for `-ENODEV`). Note: the existing
`probe.rs#cryptsetup_status_inactive` (~792) and `add.rs#mock_status_inactive` (~5077) fixtures
put the line on stderr -- that is a latent infidelity in those copies; the new fixture follows
the captured truth instead of mirroring them (see Out of scope).

```rust
/// Inactive-mapper status fixture matching real cryptsetup output: the
/// "is inactive." line lands on stdout (cryptsetup `action_status` logs it via
/// `log_std`/CRYPT_LOG_NORMAL), stderr is empty, exit is 4 (`-ENODEV`). Drives
/// classify_candidate_mapper's Inactive fail-closed skip; `parse_cryptsetup_status`
/// keys inactivity off this stdout line.
fn cryptsetup_status_inactive(mapper: &str) -> RawCommandOutput {
    RawCommandOutput {
        cmd: format!("cryptsetup status {mapper}"),
        stdout: format!("/dev/mapper/{mapper} is inactive.\n"),
        stderr: String::new(),
        exit_status: 4,
    }
}
```

### Test 1 -- Null arm (seed 707)

```rust
// Intent: a scanned braid-* candidate whose `cryptsetup status` reports a
//   null backing device is skipped -- never closed as orphan-by-name.
// Why it exists: a null-backing mapper's LUKS UUID cannot be read, so its
//   identity is unprovable; closing it by the braid-* name would be a
//   fail-open in lock teardown. Pins the BackingDevice::Null arm, whose
//   distinct error text was previously unreachable by any test.
// Scenario: seed 707 -- pool unmounted, /dev/mapper/braid-null is listed but
//   `cryptsetup status` returns an active mapping with `device: (null)`.
#[test]
fn uuid_scanned_fallback_null_backing_candidate_is_skipped() {
    let fs = lock_fs(&["/dev/mapper/braid-null"]);
    let membership = lock_test_membership();
    let runner = MockRunner::default().with_output(
        CmdRequest::CryptsetupStatus { mapper: MapperName("braid-null".into()) },
        cryptsetup_status_active_null("braid-null"),
    );
    let mut acc = CloseSetAccumulator::default();
    let close_set = build_close_sets_uuid_scanned_fallback(&runner, &fs, &membership, &mut acc);

    assert!(close_set.is_empty(), "null-backing candidate must not enter the close set");
    assert!(member_summaries(&close_set).is_empty());
    assert!(
        orphan_summaries(&close_set).is_empty(),
        "null-backing mapper must not be demoted to orphan-by-name",
    );
    assert!(acc.cleanup.is_uncertain(), "unprovable skip must mark cleanup uncertain");
    assert!(
        acc.notes.iter().any(|note| matches!(
            note,
            PreviewNote::Warn(body)
                if body.contains("skipping mapper braid-null") && body.contains("reports null")
        )),
        "null-distinct skip warn expected, got: {:?}",
        acc.notes,
    );
    assert!(
        !runner.requests().iter().any(|r| matches!(r, CmdRequest::CryptsetupLuksUuid { .. })),
        "null backing must short-circuit before any luksUUID read",
    );
}
```

### Test 2 -- Inactive arm (seed 708)

Identical structure with `cryptsetup_status_inactive("braid-gone")`, candidate `braid-gone`,
and the arm-distinct substring `mapper is inactive`:

```rust
// Intent: a scanned braid-* candidate whose `cryptsetup status` reports the
//   mapper inactive is skipped -- never closed as orphan-by-name.
// Why it exists: an inactive status (the dm slot was torn down between the
//   /dev/mapper scan and the status call) proves neither member nor orphan
//   identity; closing by name would be fail-open. Pins the
//   CryptsetupStatusOutput::Inactive arm, previously untested here.
// Scenario: seed 708 -- pool unmounted, /dev/mapper/braid-gone is listed but
//   `cryptsetup status` exits 4 with "is inactive.".
#[test]
fn uuid_scanned_fallback_inactive_candidate_is_skipped() {
    let fs = lock_fs(&["/dev/mapper/braid-gone"]);
    let membership = lock_test_membership();
    let runner = MockRunner::default().with_output(
        CmdRequest::CryptsetupStatus { mapper: MapperName("braid-gone".into()) },
        cryptsetup_status_inactive("braid-gone"),
    );
    let mut acc = CloseSetAccumulator::default();
    let close_set = build_close_sets_uuid_scanned_fallback(&runner, &fs, &membership, &mut acc);

    assert!(close_set.is_empty());
    assert!(member_summaries(&close_set).is_empty());
    assert!(
        orphan_summaries(&close_set).is_empty(),
        "inactive mapper must not be demoted to orphan-by-name",
    );
    assert!(acc.cleanup.is_uncertain());
    assert!(
        acc.notes.iter().any(|note| matches!(
            note,
            PreviewNote::Warn(body)
                if body.contains("skipping mapper braid-gone") && body.contains("mapper is inactive")
        )),
        "inactive-distinct skip warn expected, got: {:?}",
        acc.notes,
    );
    assert!(
        !runner.requests().iter().any(|r| matches!(r, CmdRequest::CryptsetupLuksUuid { .. })),
        "inactive status must short-circuit before any luksUUID read",
    );
}
```

### Why these assertions are structure-insensitive

They assert on the planner's observable outputs -- the close set, the tri-state cleanup
confidence (`is_uncertain()`), the operator-facing warn note, and the issued command requests --
not on internal control flow. Substring matches use stable, minimal tokens (`reports null`,
`mapper is inactive`) drawn from the operator-facing contract, consistent with existing tests
(`full_arm_stranded_mapper_classify_failure_skips_candidate` ~4978 matches `skipping mapper braid-stranded`).
Only `CryptsetupStatus` is seeded; both arms return before the `luksUUID` call, so no second
fixture is required.

## Out of scope (deliberately)

- **luksUUID-failure arm** (~250-252) and the two `parse_*` `map_err` arms (~229-230, ~253-254):
  these route through the *same* `Err` branch as the already-tested status-`CmdError` skip
  (`full_arm_stranded_mapper_classify_failure_skips_candidate` ~4978, via `MockRunner` MissingMock)
  and are not distinct *parse outcomes* of a successful status call, so they carry far lower
  regression risk. Not worth a dedicated test.
- **Crate-wide dedup / correction of the inactive fixture** (separate copies in `probe.rs`,
  `add.rs`, `replace.rs`, `recover.rs`): pre-existing duplication that shares no root cause with
  this coverage gap. `lock.rs` already keeps its own local `cryptsetup_status_active_null`, so a
  local `cryptsetup_status_inactive` is consistent; unifying all copies is a separate refactor.
  Note the `probe.rs`/`add.rs` copies also route the inactive line to stderr (contradicting the
  captured `.stdout` fixtures and `action_status`); correcting those is left to that refactor and
  is intentionally not bundled here -- their tests pass today because `parse_cryptsetup_status`
  scans both streams.
- The negative-assertion test at ~4762 stays untouched.

## Verification

1. **Run the new tests:**
   `just test-rust` (or `cargo test --lib --bin braid uuid_scanned_fallback_`). Both new tests,
   and the existing `uuid_scanned_fallback_*` / `full_arm_*` neighbors, pass.
2. **Confirm the warn substrings render as expected.** `skipped_mapper_warn_body`
   (`cli/src/lock.rs#skipped_mapper_warn_body` ~311) interpolates the `CmdError` detail, so the
   Null warn contains `...reports null)` and the Inactive warn contains `...mapper is inactive)`
   regardless of `CmdError`'s Display prefix (the tokens live inside the wrapped message). The
   asserts above pin exactly these.
3. **Prove the tests bite (discriminating power).** Temporarily patch the Null arm to return
   `Ok(LockMapperCloseKind::Orphan { disk_name: name_from_mapper(mapper.as_str()).unwrap_or(mapper.as_str()).to_owned() })`
   and confirm `uuid_scanned_fallback_null_backing_candidate_is_skipped` fails on the
   `orphan_summaries(...).is_empty()` / warn assertions; revert. (Optionally repeat for Inactive.)
   This is the standard "fail for the right reason" confirmation per the project's TDD norm,
   adapted to a regression-guard test for already-correct behavior.
4. No parser-fixture refresh and no `flake.nix` checks registration: these are plain `#[test]`
   functions, not NixOS VM tests. `just test-parsers` / `just capture-all-fixtures` are not
   triggered (no `nixpkgs` or parser change). The added strings are ASCII; the
   `check-output-ascii.py` gate exempts tests/comments anyway.
