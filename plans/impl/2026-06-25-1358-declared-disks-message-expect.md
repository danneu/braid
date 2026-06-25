# Plan: collapse the dead `.expect` in `summarize_declared_disks`

## Context

`summarize_declared_disks` (`cli/src/doctor.rs`) renders the doctor
`declared_disks` `CheckResult`. Its warn/fail message is built by a two-branch
`if disk_problem_count > 0 { ... } else { ... }`, and the `else` arm ends in:

```rust
topology_unavailable.expect("non-ok path with zero disk problems implies unavailable")
```

That `.expect()` is **dead by construction**. The early return at
`cli/src/doctor.rs:461`

```rust
if disk_problem_count == 0 && topology_unavailable.is_none() {
    return CheckResult::ok(...);
}
```

guarantees that any code past it satisfies `disk_problem_count > 0 ||
topology_unavailable.is_some()`. The `else` arm runs only when
`disk_problem_count == 0`, which forces `topology_unavailable.is_some()` -- so the
panic guard can never fire. It exists only to pull the `&str` out of an
`Option<&str>` the compiler can't prove is `Some`.

The cost is a maintainability smell, not a bug: the "is there anything to report"
decision is encoded once in the early return and the message shape re-tests the
same booleans, with a panic backstop a future editor must keep in sync.

**Note on the originating finding:** the finding proposed
`if parts.is_empty() { "could not compare..." } else { ... }`. That only *relocates*
the boolean test (`disk_problem_count > 0` -> `parts.is_empty()`); the
`parts.is_empty()` arm would still have to extract `reason` from `Option<&str>` and
would re-introduce an `.expect()`/`.unwrap()`. This plan pivots to a construction
that genuinely removes the panic guard.

## The change

Replace **only** the message-assembly block (`cli/src/doctor.rs:515-534`) with a
`Vec<String>` of segments joined by `"; "`. Each output line becomes a pushed
segment; `if let Some(reason)` consumes the `Option` with no panic guard.

**Before** (`cli/src/doctor.rs:515-534`):

```rust
let message = if disk_problem_count > 0 {
    let mut message = format!(
        "{}/{} {} problems: {}",
        disk_problem_count,
        total,
        if total == 1 { "disk has" } else { "disks have" },
        parts.join("; ")
    );
    if let Some(reason) = topology_unavailable {
        message.push_str(&format!(
            "; could not compare declared disks to live pool: {reason}"
        ));
    }
    message
} else {
    format!(
        "could not compare declared disks to live pool: {}",
        topology_unavailable.expect("non-ok path with zero disk problems implies unavailable")
    )
};
```

**After:**

```rust
let mut segments: Vec<String> = Vec::new();
if disk_problem_count > 0 {
    segments.push(format!(
        "{}/{} {} problems: {}",
        disk_problem_count,
        total,
        if total == 1 { "disk has" } else { "disks have" },
        parts.join("; ")
    ));
}
if let Some(reason) = topology_unavailable {
    segments.push(format!(
        "could not compare declared disks to live pool: {reason}"
    ));
}
let message = segments.join("; ");
```

Everything else in the function is untouched: the problem-vector building
(`430-452`), `disk_problem_count` (`454-459`), the early return (`461-469`),
`parts` building (`471-513`), and the warn/fail tail (`535-539`).

### Output is byte-identical in every reachable case

| Case | `disk_problem_count` | `topology_unavailable` | Result |
|---|---|---|---|
| A | `> 0` | `None` | only the problems segment -> identical to old true-branch (no append) |
| B | `> 0` | `Some(r)` | both segments, `join("; ")` inserts `"; "` between them -> identical to old `push_str("; could not compare ...")` |
| C | `== 0` | `Some(r)` | only the topology segment -> identical to old `else` branch |
| D | `== 0` | `None` | early return at `461`, never reaches this block |

The old append literal is `"; could not compare ..."` (leading `"; "`); the new
segment is `"could not compare ..."` and `join("; ")` supplies the separator -- so
the joined string matches byte-for-byte.

This table is hand-traced; the exact-message tests in *Test coverage* below turn it
into a machine-checked invariant (they run green against the pre-refactor code, then
again after).

### Why this shape, not a larger restructure

The early return is **not** redundant duplication to fold away. It makes a distinct
semantic decision -- "is the array healthy?" -> emit a *positive*
`CheckResult::ok("all {total} declared disks present")` -- whereas the segments are
*problem* phrasings feeding warn/fail. Folding the ok-return into
`segments.is_empty()` would entangle that positive, exact-pinned message with the
problem-segment logic for a marginal DRY gain. Keeping the happy-path-first early
return is both safer and semantically cleaner. The genuinely dead code is only the
`.expect()` plus the `if/else` split that existed solely to route around the
`Option` -- which is exactly what this change removes.

## Scope / blast radius

- **Files modified:** `cli/src/doctor.rs` only (one function body).
- **Single production caller:** `check_declared_disks` at `cli/src/doctor.rs:608`.
  No other callers, no public API surface, no exported types touched.
- **Test changes:** adds three exact-message unit tests in `cli/src/doctor.rs`
  (see *Test coverage*) to lock the segment-join formatting contract the refactor
  preserves. The existing branch tests assert with `.contains()` (unit:
  `cli/src/doctor.rs:3714, 3756, 3783, 3823, 3849, 3885, 4130, 4176, 4201, 4239`;
  VM: `tests/cli/braid-doctor.py`, `braid-doctor-offline-member.py`,
  `braid-doctor-uuid-swap.py` -- all substring) and stay as-is; they guard their own
  intents (guidance wording, severity dominance). The only pre-existing exact-match
  test, `summarize_ok_when_all_headers_intact` (`cli/src/doctor.rs:3686`, pins
  `"all 2 declared disks present"`), exercises the **early-return path this change
  does not touch**.
- **No doc changes.** `docs/commands/doctor.md` examples (`all 3 declared disks
  present`, `1/3 disks have problems: ...`) stay accurate because output is
  byte-identical.

## Test coverage

The current tests around the changed branches assert with `.contains()` (substring),
which cannot catch a separator (`"; "` vs `", "` vs none), segment-order, or
extra/missing-segment regression -- exactly the formatting contract this refactor
claims to preserve and that `docs/commands/doctor.md` examples reproduce. Close that
gap with three exact-message unit tests, one per reachable changed shape, added
alongside the existing `summarize_*` tests in `cli/src/doctor.rs`.

These are behavioral and structure-insensitive: the user-facing `result.message`
string *is* the contract here (not internal `Vec`-vs-`String` structure). They use
guidance-free disk states (`Offline`, `LuksHeaderOk`) so the expected strings pin
formatting structure without coupling to volatile `luks::*_guidance()` wording. Reuse
the existing `cls(name, by_id, state)` fixture (`cli/src/test_fixtures/doctor.rs#cls`)
and the `summarize_declared_disks(&inputs, topology)` call pattern, mirroring
`summarize_ok_when_all_headers_intact` (`cli/src/doctor.rs:3686`, the existing
exact-message test).

| Shape | Inputs (single `cls(...)`) | `topology_unavailable` | Status | Exact `result.message` |
|---|---|---|---|---|
| problems only (A) | `("disk1", "/dev/disk/by-id/wwn-0x1", Offline)` | `None` | Warn | `1/1 disk has problems: 1 present but not in the live pool: disk1 (/dev/disk/by-id/wwn-0x1)` |
| problems + unavailable (B) | `("disk1", "/dev/disk/by-id/wwn-0x1", Offline)` | `Some("boom")` | Warn | `1/1 disk has problems: 1 present but not in the live pool: disk1 (/dev/disk/by-id/wwn-0x1); could not compare declared disks to live pool: boom` |
| unavailable only (C) | `("disk1", "/dev/disk/by-id/wwn-0x1", LuksHeaderOk)` | `Some("boom")` | Warn | `could not compare declared disks to live pool: boom` |

Shape B is the keystone -- the only case where the join runs with **both** segments
present, so a separator or segment-order regression surfaces there. A and C pin the
single-segment shapes, catching any spurious leading/trailing separator. Each test
carries the standard `/* Intent / Why it exists / Scenario */` preamble noting it
locks the segment-join formatting contract the refactor preserves and that
`docs/commands/doctor.md` reproduces. Suggested names:
`summarize_exact_message_problems_only`,
`summarize_exact_message_problems_with_topology_unavailable`,
`summarize_exact_message_topology_unavailable_only`.

## Verification

Prove the byte-identical claim empirically by landing the tests *before* the
refactor:

1. Add the three exact-message tests (above) to `cli/src/doctor.rs`.
2. `just test-rust` against the **current** code -> all three pass, capturing
   today's exact output as a golden and confirming the expected strings are right.
3. Apply the message-assembly refactor (`cli/src/doctor.rs:515-534`).
4. `just test-rust` again -> the three new tests plus every existing `summarize_*`
   test (Shapes A/B/C, early-return ok, uuid-mismatch->fail) pass unchanged.
   Byte-identity is now machine-verified, not hand-traced.
5. `just clippy` -- confirm the `Vec<String>` + `join` construction is lint-clean
   (no needless-`mut`, etc.).
6. (Optional, not required for correctness) The NixOS VM doctor tests use substring
   assertions and are byte-identical-safe, so they need not be re-run for this
   change.
