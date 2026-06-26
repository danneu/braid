# Plan: reword `EnrollmentCandidate.uuid` doc to drop the false membership contrast

## Context

A review finding (Low / Simplicity) flagged the `EnrollmentCandidate` struct
doc in `cli/src/enroll_key_file.rs`. The doc claims the carried `uuid` lets the
execute-time re-probe "compare against the exact value discovery validated,
**rather than re-deriving it from membership** at the mutation boundary."

The verify-issue investigation confirmed the finding: the carried value is
`expected_uuid.clone()` (`enroll_key_file.rs:177`), and `expected_uuid` is
exactly the per-disk key from `membership.iter_by_name()` (`:133`). Because
`PoolMembership` is static in-memory config that never changes between planning
and `execute`, "re-deriving it from membership" would yield the *identical*
value. The comment dresses up a pure locality/plumbing convenience as a
safety/correctness distinction, which can mislead a future maintainer into
thinking the carried UUID and the membership key could diverge.

The field itself is worth keeping, but for input locality, not access:
`execute` *does* reach membership via `params.membership`
(`EnrollKeyFileParams.membership`, `:493`, passed into `execute` at `:537`), so
"re-probe without `PoolMembership`" would be a fresh false boundary claim. The
real benefit is cohesion: the re-probe loop iterates `self.candidates` and reads
`c.name`, `c.by_id`, and `c.uuid` from one place (`:553-554`), so co-locating
the expected UUID with the other re-probe inputs keeps that loop self-describing
rather than interleaving a parallel `membership` lookup keyed by disk name.

Intended outcome: a comment-accuracy fix only. No code/behavior change.

## Recommended approach

Reword the struct doc at `cli/src/enroll_key_file.rs:100-104` to frame the
benefit as input locality (the re-probe's per-candidate inputs travel together)
and drop the misleading "rather than re-deriving it from membership" contrast --
without substituting a new boundary claim about membership access. The
replacement mirrors the same
discovery-proved-equal-to-the-live-header concept already used at
`enroll_key_file.rs:788` ("discovery-validated uuid") and `:219` ("the
discovery-validated `expected` UUID"), and the honest locality framing of the
sibling field at `replace.rs:221-224`.

**Before** (`cli/src/enroll_key_file.rs:100-104`):

```rust
/// A present LUKS pool member that discovery validated as enrollable.
/// Carries the `uuid` discovery already proved equals the live header so
/// the execute-time re-probe (`reprobe_member_luks_uuid`) can compare
/// against the exact value discovery validated, rather than re-deriving
/// it from membership at the mutation boundary.
```

**After:**

```rust
/// A present LUKS pool member that discovery validated as enrollable.
/// Carries the membership-expected `uuid` that discovery proved equal to
/// the live header, keeping the execute-time re-probe's inputs together.
```

Why this wording:
- Keeps line 1 (accurate) verbatim.
- "membership-expected `uuid`" states plainly that the carried value *is* the
  membership key, removing any implication it could differ; "that discovery
  proved equal to the live header" preserves the discovery invariant.
- "keeping the execute-time re-probe's inputs together" names the actual benefit
  -- cohesion/input locality -- without any access claim. The re-probe loop
  (`for c in &self.candidates { reprobe_member_luks_uuid(.., &c.name, &c.by_id,
  &c.uuid) }`, `:553-554`) sources all three inputs from one `c`; it makes no
  claim that membership is unreachable (it is -- via `params.membership`).
- Three lines, within the "Prefer one to three lines" guidance in
  `docs/dev/doc-comments.md`; ASCII-only; still explains *why* the `pub` field
  exists at its boundary.

## Critical files

- `cli/src/enroll_key_file.rs` -- the only file changed (struct doc at
  `:100-104`). No other site touched.

## Reuse / consistency references (no edits)

- `cli/src/enroll_key_file.rs:219` -- "the discovery-validated `expected` UUID"
  (accurate sibling doc on `reprobe_member_luks_uuid`).
- `cli/src/enroll_key_file.rs:788` -- existing "discovery-validated uuid"
  phrasing.
- `cli/src/replace.rs:221-224` -- analogous always-populated field documented as
  honest locality ("without re-deriving from `paths`"), the model this reword
  follows.

## Verification

Comment-only change with no behavioral impact, so the bar is "nothing broke":

1. `just test-rust` -- existing tests still pass (the justfile notes the crate
   is `braid-cli`, so prefer this recipe over `cargo test -p <name>`). The
   behavior the comment describes is already pinned by
   `reprobe_member_luks_uuid_mismatch_rejects`,
   `reprobe_member_luks_uuid_probe_failure_fails_closed`, and the
   `execute_rejects_swapped_*_before_mutation` tests
   (`enroll_key_file.rs:3637+`); no test edits are needed.
2. `cargo fmt --check` and `just clippy` clean.

No new tests: there is no behavioral surface to cover, and the existing re-probe
tests already guard the invariant the comment now states accurately.
