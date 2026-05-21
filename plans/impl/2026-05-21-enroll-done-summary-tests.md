# Pin `braid enroll` `done:` summary with tests

## Context

`apply_enrollment` in `cli/src/enroll_key_file.rs:321-332` always
emits `done: N enrolled, M already had keyfile` after the apply loop.
This line is a batch-level confirmation: it summarizes the
enrollment outcome across the whole pool in one place, distinct from
the per-disk `[ok]` closers
(`probe_keyfile_enrollment` for `AlreadyEnrolled`,
`disk <name>: keyfile enrolled in slot 1` for `NeedsEnroll`). The
summary tells the operator at a glance how many disks were mutated
versus how many were already up to date, without having to count
interleaved per-disk rows.

The line is asserted nowhere in the test suite (verified via
`grep "done:" tests/`), so any future regression that drops it,
reorders the counts, or changes the wording would silently land. This
plan is test-only hardening: pin the existing user-visible wording on
both the mutation path and the fully-idempotent re-enroll path so the
summary's shape is part of the contract.

No Rust changes. The summary stays as-is.

## Change

`tests/cli/braid-enroll.py` only. Add two positive assertions to
existing tests that already capture stderr -- no new test cases or
fixtures.

**Test 1 (initial enroll, mutation path, lines 80-140)** -- both
disks become `NeedsEnroll`, so the summary is
`done: 2 enrolled, 0 already had keyfile`. `t1_err` is already
captured at line 93. Add after the existing enrolling-wait /
enrolled-ok closer-pair checks:

```python
assert "done: 2 enrolled, 0 already had keyfile" in t1_err, (
    f"expected 'done: 2 enrolled, 0 already had keyfile' summary "
    f"on mutation path, got: {t1_err!r}"
)
```

**Test 3 (idempotent re-enroll, lines 205-221)** -- both disks
become `AlreadyEnrolled`, so the summary is
`done: 0 enrolled, 2 already had keyfile`. `t3_err` is already
captured at line 214. Add after the existing `assert_ordered_pair`
loop:

```python
assert "done: 0 enrolled, 2 already had keyfile" in t3_err, (
    f"expected 'done: 0 enrolled, 2 already had keyfile' summary "
    f"on fully-idempotent re-enroll, got: {t3_err!r}"
)
```

Do not add an assertion for the mixed `enrolled > 0, already > 0`
case: no existing test constructs it (it requires a real pool where
slot 1 is occupied on some disks but empty on others), and inventing
one solely to anchor a string is structure-sensitive busywork.

## Verification

`just test-vm braid-enroll` -- runs the full braid-enroll VM test,
exercising both new assertions. `just test-rust` is unnecessary
because no Rust code changes; the existing unit tests in
`cli/src/enroll_key_file.rs` are unaffected.
