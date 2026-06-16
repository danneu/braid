# Plan: pin enroll discovery short-circuit on the first UUID mismatch

## Context

`discover_enrollment_candidates` (`cli/src/enroll_key_file.rs:131-179`) iterates
membership by name and, on the first member whose live LUKS UUID at its by-id path
no longer matches the membership key, `return`s a validation error immediately
(`:159-171`). This fail-closed short-circuit is mandated by ADR 024 (re-check member
identity at every mutation boundary): a swapped/reformatted disk must abort *before*
probing or mutating any further member.

The behavior is correct but **unpinned**. Nothing today proves discovery stops at the
*first* mismatch without probing later members:

- The existing unit test `discover_rejects_luks_uuid_mismatch_before_slot_inventory`
  (`cli/src/enroll_key_file.rs:940-1012`) puts the mismatch on disk1 (first in name
  order) but only asserts that no slot-inventory (`CryptsetupLuksDump`) and no mutation
  (`CryptsetupLuksAddKeyFile`) ran. Those two negatives are **insensitive to loop
  position**: the test calls `discover_enrollment_candidates` directly and never invokes
  the planning/apply phases that issue those commands, so a "collect all problems"
  regression that kept looping would still pass them.
- `MockRunner` (`cli/src/cmd.rs:1471-1589`) stores outputs in a `HashMap` with no
  consumption check and no `Drop` assertion, so the test's registered-but-unused disk2
  mocks do **not** implicitly assert disk2 is unprobed.
- The other mismatch test, `preserved_context_failure_returns_notes_in_name_order`,
  puts the mismatch on the *last* member and targets note ordering.
- The VM test `tests/cli/enroll-uuid-mismatch.py` reformats disk2 (the last of two
  disks), so there is no later member to over-probe.

A regression that accumulated mismatches and continued the loop would issue extra
`CryptsetupLuksUuid` / `CryptsetupLuksDumpText` probes against subsequent members
(and could surface a confusing multi-disk error) while passing every existing test.

The sibling loop in `mount.rs` (`:222-283`, same `iter_by_name` + fail-closed-on-mismatch
shape) **does** pin its position invariant via
`plan_open_pool_emits_events_before_uuid_mismatch_on_later_member` (`mount.rs:2054-2109`),
which asserts disk1's probe event is the only event because "disk2 returns before pushing
its own." Enroll is the lone mutation-boundary discovery loop missing the mirror coverage.

**Outcome:** add one focused unit test that pins the short-circuit at the command-issuance
level, restoring parity with `mount.rs`.

## The fix (test-only)

Add a sibling test in the `#[cfg(test)] mod tests` block of
`cli/src/enroll_key_file.rs`, immediately after
`discover_rejects_luks_uuid_mismatch_before_slot_inventory` (~line 1012) and before
`preserved_context_failure_returns_notes_in_name_order`. Suggested name:
`discover_stops_at_first_uuid_mismatch_without_probing_later_members`.

Use a **3-disk** membership where disk1 (first in name order) mismatches and disk2/disk3
are fully healthy LUKS members. Assert that `runner.requests()` contains **no command
targeting disk2's or disk3's by-id path** -- proving the loop returned at disk1 and never
reached the later members.

### Test sketch

```rust
// Intent: discovery aborts at the FIRST UUID-mismatched member and probes
//   no later member -- the loop short-circuits, it does not "collect all
//   problems".
// Why it exists: ADR 024 requires fail-closed identity re-checks at the
//   mutation boundary. The early return on mismatch was unpinned: a
//   regression that accumulated mismatches and kept looping would issue
//   extra CryptsetupLuksUuid/CryptsetupLuksDumpText probes against later
//   members (and risk a confusing multi-disk error) while passing every
//   existing test. mount.rs pins the mirror invariant
//   (plan_open_pool_emits_events_before_uuid_mismatch_on_later_member);
//   enroll did not.
// Scenario: 3-disk pool. disk1's by-id path now points at a foreign LUKS
//   container (swap/reformat) while disk2 and disk3 are healthy. Discovery
//   must fail on disk1 without touching disk2 or disk3.
#[test]
fn discover_stops_at_first_uuid_mismatch_without_probing_later_members() {
    let d1 = "/dev/disk/by-id/d1";
    let d2 = "/dev/disk/by-id/d2";
    let d3 = "/dev/disk/by-id/d3";
    // enroll_make_membership assigns test_uuid(500 + idx) by position:
    // disk1 -> 500, disk2 -> 501, disk3 -> 502.
    let observed_d1 = "ffffffff-ffff-ffff-ffff-ffffffffffff"; // != test_uuid(500)

    // disk2/disk3 are registered as healthy LUKS members on purpose: they
    // are fully probe-able, so the ONLY reason they go untouched is the
    // short-circuit at disk1. (MockRunner is lenient about unconsumed mocks.)
    let (u1, o1) = enroll_luks_uuid_ok(d1, observed_d1);
    let (u2, o2) = enroll_luks_uuid_ok(d2, test_uuid(501).as_str());
    let (u3, o3) = enroll_luks_uuid_ok(d3, test_uuid(502).as_str());
    let runner = MockRunner::default()
        .with_output(u1, o1)
        .with_output(u2, o2)
        .with_output(u3, o3)
        .with_luks_dump_text_luks2(d1)
        .with_luks_dump_text_luks2(d2)
        .with_luks_dump_text_luks2(d3)
        .with_mappers_closed(&["braid-disk1", "braid-disk2", "braid-disk3"]);
    let fs = enroll_fs(&[d1, d2, d3]);
    let membership =
        enroll_make_membership(&[("disk1", d1), ("disk2", d2), ("disk3", d3)]);

    let (notes, result) = discover_enrollment_candidates(
        &runner,
        &fs,
        &membership,
        crate::test_fixtures::mock_virtio_backing_path_resolver(),
    );

    assert!(notes.is_empty(), "unexpected notes: {notes:?}");
    let err = result.expect_err("first-member UUID mismatch must reject discovery");
    assert!(
        matches!(&err, EnrollKeyFileError::Validation(m) if m.contains("disk1")
            && m.contains("LUKS UUID mismatch")),
        "error should be the disk1 UUID-mismatch refusal: {err:?}"
    );

    let requests = runner.requests();
    // disk1 (the mismatched first member) WAS probed.
    assert!(
        requests.iter().any(
            |r| matches!(r, CmdRequest::CryptsetupLuksUuid { device } if device == d1)
        ),
        "disk1 must be probed: {requests:?}"
    );
    // No later member is touched by ANY device-bearing command: the loop
    // returned at disk1. CryptsetupLuksUuid is the first command
    // probe_config_disk issues for a present disk, so the absence of any
    // by-id read proves the member was never reached.
    let touches = |device: &str| {
        requests.iter().any(|r| match r {
            CmdRequest::CryptsetupLuksUuid { device: dev }
            | CmdRequest::CryptsetupLuksDumpText { device: dev }
            | CmdRequest::CryptsetupLuksDump { device: dev }
            | CmdRequest::CryptsetupLuksAddKeyFile { device: dev, .. } => dev == device,
            _ => false,
        })
    };
    assert!(
        !touches(d2),
        "discovery must not probe disk2 after the first mismatch: {requests:?}"
    );
    assert!(
        !touches(d3),
        "discovery must not probe disk3 after the first mismatch: {requests:?}"
    );
}
```

(The exact assertion wording can mirror the richer mismatch-message checks from
`discover_rejects_luks_uuid_mismatch_before_slot_inventory` if desired; the load-bearing
new assertions are the two `!touches(...)` checks.)

## Design decisions

- **Sibling test, not an extension of the existing one.** The existing test pins a
  *different* invariant ("discovery error precedes the planning-phase slot probe").
  Overloading it would blur single-behavior-per-test clarity. A focused sibling documents
  the short-circuit invariant and mirrors `mount.rs`'s dedicated position test.
- **3 disks, assert both later members unprobed.** Two disks (disk1 mismatch, assert
  disk2 unprobed) already proves "stops at first," but asserting *both* disk2 and disk3
  are untouched faithfully captures the "later members" (plural) invariant and rules out
  a one-member overrun. The finding's literal proposal -- assert only disk3 -- would miss
  a regression that probes disk1 then disk2 then stops; `touches(d2)` closes that.
- **Register disk2/disk3 as healthy LUKS.** Makes the negative assertion meaningful: the
  later members are fully probe-able, so the only reason they are untouched is the
  short-circuit -- not a missing mock. (`MockRunner` is lenient about unconsumed outputs.)
- **Assert on `runner.requests()` device paths.** Behavioral and structure-insensitive:
  it observes which subprocess invocations would occur, not how discovery is implemented.
  This is the same idiom the existing test and `mount.rs` use. The `touches` closure keys
  off the by-id `device` field, robust to future probe steps added inside
  `probe_config_disk`. (`CryptsetupStatus` keys off the mapper name, not by-id, but since
  the by-id `CryptsetupLuksUuid` read is the *first* command `probe_config_disk` issues
  for a present disk, its absence already proves the member was never reached.)
- **VM test stays as-is (out of scope).** `tests/cli/enroll-uuid-mismatch.py` observes
  end-to-end state (no keyfile created, slot 1 empty, messaging) and structurally *cannot*
  observe which probes ran or their order. The short-circuit is a unit-level invariant;
  contorting the VM test (extra disk, full-VM boot cost) to chase it would be the wrong
  level. Deliberate scoping decision, not an oversight.

## Optional consistency extension (`mount.rs`)

`mount.rs` has the symmetric gap: `plan_open_pool_emits_events_before_uuid_mismatch_on_later_member`
covers a mismatch on the *last* member (earlier healthy members processed, their events
survive), but there is no test for a mismatch on the *first* member proving later members
go unprobed. If the user wants full parity across both mutation-boundary discovery loops,
add a mirror test to `mount.rs` asserting `report.events` is empty (or contains only the
mismatch context) and that no later member's by-id path appears in `runner.requests()`
when disk1 mismatches. Left optional because the finding is scoped to enroll and mount
already has solid (opposite-direction) position coverage.

## Files to modify

- `cli/src/enroll_key_file.rs` -- add the one test function in the `tests` module. All
  fixtures used (`enroll_luks_uuid_ok`, `with_luks_dump_text_luks2`, `with_mappers_closed`,
  `enroll_fs`, `enroll_make_membership`, `test_uuid`, `mock_virtio_backing_path_resolver`,
  `CmdRequest`, `MockRunner`) are already imported in that block.
- (Optional) `cli/src/mount.rs` -- the mirror test described above.

No production code changes.

## Verification

1. **Run the new test:**
   `just test-rust` (full Rust suite), or targeted:
   `cargo test -p braid discover_stops_at_first_uuid_mismatch_without_probing_later_members`
   (adjust the package flag to the crate's actual name if needed). Expect green.
2. **Confirm it fails for the right reason (TDD guard, per AGENTS.md).** Temporarily edit
   the discovery loop at `cli/src/enroll_key_file.rs:159-171` so the mismatch arm does NOT
   return early (e.g. push to a list and `continue`). Re-run the test: it must fail on the
   `!touches(d2)` assertion (proving the assertion actually guards the short-circuit), and
   the pre-existing `discover_rejects_luks_uuid_mismatch_before_slot_inventory` should still
   pass (confirming it does not catch this regression). Revert the production edit.
3. **No regressions:** `just test-rust` stays green overall.
