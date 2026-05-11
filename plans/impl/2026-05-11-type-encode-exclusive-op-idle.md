# Plan: eliminate `ExclusiveOp::None` -- type-encode idle vs. busy

## Context

`cli/src/idle.rs:112-125` carries a defensive arm in `busy_from_exclop`
that maps `ExclusiveOp::None` to `BusyReason::Balance`. The arm is
unreachable today -- `check_any_btrfs_exclusive_op` filters
`ExclusiveOp::None` out before raising `Busy(op)` -- but if that guard
is ever weakened, `braid idle` would print "busy: balance running" while
the kernel actually reports "none". That diverges from the rest of the
file's fail-closed discipline, where every other unknown-state path
surfaces as `BusyReason::Unknown(msg)` via `busy_unknown`.

A `verify-issue` review of that finding showed the local fix (rewrite
the arm to `Unknown(...)` + a unit test pinning the invariant) leaves
the same shape behind at two sibling call sites in `preflight.rs`:

- `cli/src/preflight.rs:185-188` -- `check_no_exclusive_op` does
  `match op { ExclusiveOp::None => Ok(()), _ => Err(Busy(op)) }`.
- `cli/src/preflight.rs:236-240` -- `check_any_btrfs_exclusive_op` has
  `if op != ExclusiveOp::None { return Err(Busy(op)) }`.

All three are the same runtime guard: "the parser returned a variant
that means 'no op'; filter it out before treating the rest of the enum
as an actual op." The `None` variant in `ExclusiveOp` models the
*absence* of a busy op as a member of the busy-op enum, which forces
every consumer to peel it off.

Outcome: remove `ExclusiveOp::None` entirely; reshape
`ExclusiveOp::parse` to return `Result<Option<Self>, String>`
(`Ok(None)` = idle, `Ok(Some(op))` = busy, `Err(s)` = unrecognized
value). Idle vs. busy becomes a type-level distinction, the defensive
arm in `idle.rs` ceases to exist (its match becomes exhaustive without
it), and the two sibling guards in `preflight.rs` collapse into the
parser's normal control flow.

This is the pivot recommended in `verify-issue` -- the finding's
proposed local patch is dropped in favor of the structural fix that
dissolves the class.

## Critical files

- `cli/src/preflight.rs` -- enum, parser, Display, two helper bodies,
  one unit test.
- `cli/src/idle.rs` -- one match arm + its comment.

No other files reference `ExclusiveOp` or its parser
(`cli/tests/`, `tests/`, `modules/`, `docs/decisions/`, and
`cli/src/test_fixtures/` were swept and contain no usage). The only
decision-doc mention is `docs/decisions/016-auto-suspend.md:57`
referring to `ExclusiveOpError::Read`, which is unaffected.

## Changes

### 1. `cli/src/preflight.rs:61-79` -- enum + boundary doc

Drop the `None` variant AND rewrite the enum-level doc comment so it
no longer frames `ExclusiveOp` as the full sysfs state. After this
plan, `ExclusiveOp` is the recognized busy-op set; the `"none"`
sentinel lives in the parser's return type, not in the enum.

```rust
/// Recognized btrfs exclusive busy operations, as reported by
/// `/sys/fs/btrfs/{fsid}/exclusive_operation`. Does not include the
/// kernel's `"none"` sentinel -- absence of a busy op is modeled as
/// `Ok(None)` from [`ExclusiveOp::parse`], so consumers cannot
/// accidentally treat idle as a member of this enum.
///
/// String values follow `exclop_def[]` in btrfs-progs
/// `common/utils.c:1186-1194` (vendored in `reference/btrfs-progs/`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExclusiveOp {
    Balance,
    BalancePaused,
    DeviceAdd,
    /// The kernel writes "device remove" -- not "device delete" as
    /// btrfs-man5.rst sometimes says. Follows `exclop_def[]` in
    /// `reference/btrfs-progs/common/utils.c:1191`.
    DeviceRemove,
    DeviceReplace,
    Resize,
    SwapActivate,
}
```

The cited reference still describes the busy values authoritatively;
only the framing changes from "full sysfs state" to "recognized busy
ops, with idle modeled outside the enum."

### 2. `cli/src/preflight.rs:81-95` -- parser

Change the signature so idle and unrecognized are distinct outcomes:

```rust
impl ExclusiveOp {
    /// Parse a single value from `/sys/fs/btrfs/{fsid}/exclusive_operation`.
    /// Expects caller-trimmed input (matches today's two call sites,
    /// which already pass `contents.trim()`); does not trim
    /// internally, so a stray trailing `\n` will fall through to the
    /// `Err` arm.
    ///
    /// `Ok(None)` = the kernel reports `"none"` (no exclusive op in
    /// flight). `Ok(Some(op))` = a recognized busy op. `Err(s)` = the
    /// kernel wrote a string we do not recognize; `s` is the input
    /// as provided, suitable for surfacing via
    /// `ExclusiveOpError::Unrecognized(s)` without re-allocating.
    pub fn parse(s: &str) -> Result<Option<Self>, String> {
        match s {
            "none" => Ok(None),
            "balance" => Ok(Some(Self::Balance)),
            "balance paused" => Ok(Some(Self::BalancePaused)),
            "device add" => Ok(Some(Self::DeviceAdd)),
            "device remove" => Ok(Some(Self::DeviceRemove)),
            "device replace" => Ok(Some(Self::DeviceReplace)),
            "resize" => Ok(Some(Self::Resize)),
            "swap activate" => Ok(Some(Self::SwapActivate)),
            other => Err(other.to_owned()),
        }
    }
}
```

The `Err` variant owns the input string so callers can pass it
straight into `ExclusiveOpError::Unrecognized(s)` without an extra
`to_owned()`. Trimming stays the caller's job, same as today -- both
call sites already do `ExclusiveOp::parse(contents.trim())`.

### 3. `cli/src/preflight.rs:97-110` -- Display

Remove the `Self::None => write!(f, "none")` arm. The remaining arms
are exhaustive against the new enum. The existing test
`exclusive_op_display` does not assert on `None`, so it stays valid.

### 4. `cli/src/preflight.rs:177-189` -- `check_no_exclusive_op`

Public signature (`Result<(), ExclusiveOpError>`) is unchanged.
Internals use the new parser shape:

```rust
pub(crate) fn check_no_exclusive_op<F: Filesystem + ?Sized>(
    fs: &F,
    fsid: &str,
) -> Result<(), ExclusiveOpError> {
    let path = format!("/sys/fs/btrfs/{fsid}/exclusive_operation");
    let contents = fs.read_to_string(&path).map_err(ExclusiveOpError::Read)?;
    match ExclusiveOp::parse(contents.trim()) {
        Ok(None) => Ok(()),
        Ok(Some(op)) => Err(ExclusiveOpError::Busy(op)),
        Err(s) => Err(ExclusiveOpError::Unrecognized(s)),
    }
}
```

The `match op { ExclusiveOp::None => Ok(()), _ => Err(...) }` split at
lines 185-188 collapses into the parser's `Ok(None) / Ok(Some)` shape.

### 5. `cli/src/preflight.rs:222-249` -- `check_any_btrfs_exclusive_op`

Same shape inside the loop:

```rust
let contents = fs.read_to_string(&path).map_err(ExclusiveOpError::Read)?;
found_fsid_dir = true;
match ExclusiveOp::parse(contents.trim()) {
    Ok(None) => continue,
    Ok(Some(op)) => return Err(ExclusiveOpError::Busy(op)),
    Err(s) => return Err(ExclusiveOpError::Unrecognized(s)),
}
```

The `if op != ExclusiveOp::None { return Err(Busy(op)) }` runtime guard
disappears.

### 6. `cli/src/idle.rs:112-125` -- `busy_from_exclop`

Remove the defensive arm and its comment. The match is now exhaustive
against the new enum, and the compiler enforces what the comment used
to assert:

```rust
fn busy_from_exclop(op: ExclusiveOp) -> BusyReason {
    match op {
        ExclusiveOp::Balance => BusyReason::Balance,
        ExclusiveOp::BalancePaused => BusyReason::BalancePaused,
        ExclusiveOp::DeviceAdd => BusyReason::DeviceAdd,
        ExclusiveOp::DeviceRemove => BusyReason::DeviceRemove,
        ExclusiveOp::DeviceReplace => BusyReason::DeviceReplace,
        ExclusiveOp::Resize => BusyReason::Resize,
        ExclusiveOp::SwapActivate => BusyReason::SwapActivate,
    }
}
```

### 7. `cli/src/preflight.rs:604-643` -- parser unit tests

Rewrite both parser tests AND their preambles so the `// Intent /
Why / Scenario` lines describe the new contract (`Ok(None)` for the
`"none"` sentinel, `Err(s)` for unrecognized input), not the old
`Option<Self>` shape.

`exclusive_op_parse_all_variants` becomes:

```rust
#[test]
// Intent: ExclusiveOp::parse maps every sysfs string from exclop_def[]
//   to the right outcome -- `"none"` -> Ok(None) (idle), each busy
//   string -> Ok(Some(variant)).
// Why: Pins the type-level split between idle and busy. If a kernel
//   string is added or renamed, this catches it before the busy paths
//   silently misclassify.
// Scenario: Kernel writes each possible exclusive_operation value.
fn exclusive_op_parse_all_variants() {
    assert_eq!(ExclusiveOp::parse("none"), Ok(None));
    assert_eq!(ExclusiveOp::parse("balance"), Ok(Some(ExclusiveOp::Balance)));
    assert_eq!(
        ExclusiveOp::parse("balance paused"),
        Ok(Some(ExclusiveOp::BalancePaused))
    );
    assert_eq!(
        ExclusiveOp::parse("device add"),
        Ok(Some(ExclusiveOp::DeviceAdd))
    );
    assert_eq!(
        ExclusiveOp::parse("device remove"),
        Ok(Some(ExclusiveOp::DeviceRemove))
    );
    assert_eq!(
        ExclusiveOp::parse("device replace"),
        Ok(Some(ExclusiveOp::DeviceReplace))
    );
    assert_eq!(ExclusiveOp::parse("resize"), Ok(Some(ExclusiveOp::Resize)));
    assert_eq!(
        ExclusiveOp::parse("swap activate"),
        Ok(Some(ExclusiveOp::SwapActivate))
    );
}
```

`exclusive_op_parse_unrecognized` becomes:

```rust
#[test]
// Intent: ExclusiveOp::parse returns Err(s) carrying the unrecognized
//   input for any value outside exclop_def[].
// Why: Future kernel versions may add new op types; fail-closed is
//   safer. Carrying the offending string lets callers surface it via
//   `ExclusiveOpError::Unrecognized` without re-allocating.
// Scenario: Kernel writes a value not in exclop_def[].
fn exclusive_op_parse_unrecognized() {
    assert_eq!(
        ExclusiveOp::parse("something new"),
        Err("something new".to_string())
    );
    assert_eq!(ExclusiveOp::parse(""), Err(String::new()));
}
```

`exclusive_op_display` stays as-is. No new tests needed; the
exhaustive `match` in `busy_from_exclop` is the new compile-time
guarantee that supersedes the runtime test the original finding asked
for.

## What this preserves

- `check_no_exclusive_op` and `check_any_btrfs_exclusive_op` keep their
  public signatures, so their callers
  (`check_exclusive_op_with_policy` at `preflight.rs:150-170`, used in
  `add`/`remove`/`replace`/`lock` preflight, and `cmd_idle`) do not
  need to change.
- `ExclusiveOpError` is unchanged.
- The `exclop_def[]` mapping to kernel strings is preserved verbatim
  -- the parser still recognizes the same eight strings (including
  `"none"`); only how it reports them changes.
- The `Display` for `ExclusiveOp::None` is dropped, but it was never
  reachable in any error or user-facing message path (the only
  formatters are in `Busy(op)` error messages and the policy helper's
  `cannot lock: {op}`, both of which only see busy variants).

## Verification

- `just test-rust` -- exercises the parser unit tests and the
  `check_no_exclusive_op` test suite directly in `preflight.rs`,
  plus the `cmd_idle` test suite in `idle.rs`, which is where
  `check_any_btrfs_exclusive_op` is exercised (end-to-end through
  `cmd_idle`; `preflight.rs` has no standalone test block for it).
  The `idle.rs` suite covers every kernel exclop string and every
  fail-closed surface and is the behavioural regression net: if any
  reshape of the match loses a busy-state variant,
  `busy_when_balance` / `busy_when_device_remove` / etc. fail; if a
  fail-closed seam reopens, `busy_unknown_on_unrecognized_exclop`
  and friends fail. The parser-signature change is the only place a
  unit test must literally change.
- `cargo check -p braid-cli` -- the exhaustive `match` in
  `busy_from_exclop` becomes a compile error if `ExclusiveOp::None` is
  reintroduced, which is the type-system replacement for the runtime
  test the original finding proposed.

No VM run is required for this refactor -- the change is purely
type-level and is fully exercised by the Rust unit tests above. Live
sysfs coverage exists for follow-up confidence (`tests/cli/braid-idle.py`
for the read path, `tests/cli/replace-inhibits-suspend.py` for the
in-flight sysfs busy-state path, plus the `add-`, `remove-`, and
`remove-missing-inhibits-suspend.py` siblings), but those are not
load-bearing for this change.

No fixture refresh, no docs/decisions update, no NixOS module change.
