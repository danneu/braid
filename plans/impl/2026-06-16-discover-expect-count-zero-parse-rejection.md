# Reject `discover --expect-count 0` at parse time

## Context

The `discover --write --expect-count N` flag is a fail-closed guard: it refuses
to write `pool.json` unless discovery produces exactly `N` braid-labeled LUKS2
members, catching a momentarily detached disk or a stray extra disk during a
rebuild. The flag currently accepts `--expect-count 0`, but `0` can never
succeed:

- If the scan finds 0 members, the `members.is_empty()` short-circuit at
  `cli/src/main.rs:967` prints `NoMembersDiscovered` and exits 1 *before*
  `write_discovered_membership` is ever called, so the count check at
  `cli/src/discover.rs:642` is unreachable.
- If the scan finds `N > 0` members, `expected_count = Some(0)` fails the
  `actual != expected` check and returns `ExpectCountUnmet { expected: 0, .. }`.

So every count value is honored except `0`, which is silently dead. The contract
is quietly inconsistent. There is no real use for an empty `pool.json` (a pool
with no disks is unmanageable, and `NoMembersDiscovered` already handles the
"nothing attached" case with a remediation message), so the fix is to reject `0`
at parse time -- turning a confusing runtime mismatch into an immediate, explicit
usage error.

This mirrors the existing `deadline_secs` range guard on `lock`
(`cli/src/main.rs:271`) in *behavior*. It cannot mirror it *verbatim*: an
upstream-source check of clap 4.6 confirmed that `clap::value_parser!(usize)`
resolves through autoref specialization to a `FromStr`-based parser
(`_AnonymousValueParser`) that has **no `.range()` method** -- clap implements
`ValueParserFactory` only for the fixed-width integers (`u8..u64`, `i8..i64`),
not `usize`/`isize`. So the originally proposed `value_parser!(usize).range(1..)`
does not compile. The pivot keeps the natural `usize` count type and names the
ranged parser directly.

Only one CLI arg gains a guard, not a sweep: of braid's three numeric CLI args,
`deadline_secs` is already guarded, and the two `missing_id` fields take btrfs
devids (opaque `u64` where `0` is not provably nonsensical -- btrfs uses devid 0
as the replace-target sentinel), so guarding those would be wrong.

The guard has two downstream carries beyond the CLI code, though. A registered
VM test (`tests/module/pool-lock-precedes-state-read.py`) deliberately uses
`discover --write --expect-count 0` as a fail-closed sentinel -- a guaranteed
refusal that still runs the lock acquire and state reads first. A parse-time
guard makes `0` exit (code 2) before the lock/probe, breaking that test's
lock-precedes-reads ordering assertions, so the sentinel must move to a
positive-but-impossible count. And the command reference
(`docs/commands/discover.md#flags`) documents `--expect-count <N>` without a
lower bound, so it must pick up the `N >= 1` contract to stay in step with the
CLI help and parse behavior.

## Change 1: range-guard the `expect_count` arg

In `cli/src/main.rs`, `DiscoverArgs` (~line 426-440). Add the ranged parser and
a brief implementation note explaining why the `value_parser!` macro form does
not apply here (this preempts both a re-introduction of the non-compiling macro
form and a future "why isn't this like `deadline_secs`?" review comment). Tighten
the doc line to state `N >= 1`.

```rust
#[derive(Debug, Args)]
struct DiscoverArgs {
    /// Write discovered membership to pool.json
    #[arg(long)]
    write: bool,
    /// Fail closed unless discovery produces exactly N (N >= 1) members.
    /// Use as a guard for any discover --write rebuild where the
    /// expected member count is known ahead of time, so a momentarily
    /// detached disk (loose cable, USB power glitch, udev race) or
    /// extra braid-labeled disk cannot silently produce the wrong
    /// pool.json.
    /// Requires --write.
    // A count is naturally usize (it is compared against members.len()).
    // clap implements ValueParserFactory for the fixed-width ints but not
    // usize, so value_parser!(usize) yields a FromStr parser with no
    // .range(); name the ranged parser directly. Rejecting 0 closes the gap
    // where --expect-count 0 could never succeed: a 0-member scan is
    // intercepted by NoMembersDiscovered before the count check, so every
    // count is honored except 0, which is dead.
    #[arg(
        long = "expect-count",
        value_name = "N",
        requires = "write",
        value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..)
    )]
    expect_count: Option<usize>,
}
```

Why this compiles and stays `usize`: `RangedU64ValueParser<T: TryFrom<u64>>`
(`clap_builder` value_parser.rs) accepts `T = usize` (`usize: TryFrom<u64>`),
its `.range()` takes `RangeBounds<u64>` (so `1..` is fine), and it yields
`usize` to match the field. No change to `write_discovered_membership`
(`cli/src/discover.rs`), its `expected_count: Option<usize>` parameter, the
`actual != expected` comparison, or the `ExpectCountUnmet { expected, actual }`
variant -- the count stays `usize` end-to-end with no cast.

## Change 2: add a parse-time rejection test

In `cli/src/main.rs`, in the `#[cfg(test)]` module, next to the existing
`discover_write_with_expect_count_parses` and
`discover_expect_count_without_write_rejected` (~line 1940-1961). It pairs with
`lock_deadline_zero_rejected` (~line 1927) and uses the house
`// Intent / Why it exists / Scenario` preamble. `ErrorKind` is already imported
in this module.

```rust
// Intent: `discover --write --expect-count 0` is rejected at parse time with a
//   value-validation error, not silently accepted into a guard that can never
//   pass.
// Why it exists: expect_count 0 is degenerate -- a 0-member scan is refused by
//   NoMembersDiscovered before the count check, so every count is honored except
//   0, which is unreachable. The range(1..) guard turns that silent dead value
//   into an explicit, immediate parse error (mirrors lock_deadline_zero_rejected).
// Scenario: operator fat-fingers `--expect-count 0` (or a script passes a bad
//   default) during a rebuild and gets a clear refusal instead of a confusing
//   "no braid-labeled LUKS2 devices found" exit 1.
#[test]
fn discover_expect_count_zero_rejected() {
    let err = Cli::try_parse_from(["braid", "discover", "--write", "--expect-count", "0"])
        .expect_err("expect-count must be >= 1");
    assert_eq!(err.kind(), ErrorKind::ValueValidation);
}
```

`--write` is included so the `requires = "write"` constraint is satisfied and the
range violation is the only error (otherwise clap returns
`MissingRequiredArgument` first). This is behavioral and structure-insensitive --
it asserts the CLI contract via `Cli::try_parse_from`, matching the existing
sibling tests.

## Change 3: move the VM sentinel off `--expect-count 0`

`tests/module/pool-lock-precedes-state-read.py` (registered as a check in
`flake.nix`) uses `discover --write --expect-count 0` as a fail-closed sentinel
in two places: the no-contention baseline (~line 112) and the lock-contention
case (~line 120). The sentinel's whole point is a *guaranteed* refusal that
nonetheless runs the lock acquire and state reads first, so the assertions can
prove ordering. The new parse guard makes `0` exit at parse time (code 2)
*before* the lock/probe, so both invocations would stop reaching their
assertions -- the baseline would no longer print `NoMembersDiscovered`, and the
contention case would no longer reach the lock and print the contention message.

Fix: change both `--expect-count 0` invocations to a positive-but-impossible
count, `--expect-count 9999`. This is a faithful replacement -- `9999` parses
(>= 1), and discovery can never produce 9999 members, so the fail-closed
property is preserved:

- Baseline (0 discoverable members): `members.is_empty()` short-circuits to
  `NoMembersDiscovered` at `cli/src/main.rs:967` *before* the count check, so the
  `refusal in base_out` assertion still passes. If fixture drift makes a member
  appear, `write_discovered_membership` refuses with `ExpectCountUnmet`
  (count != 9999) before `save_membership`, tripping the "precondition broken"
  assertion loudly -- exactly as `0` did.
- Contention: `9999` parses, discover proceeds to the nonblocking flock acquire,
  fails fast with the contention message, and exits before the pending-op /
  probe reads -- so the ordering assertions hold.

Update the explanatory comment above the baseline (~lines 107-111) to reference
`9999` instead of `0` (`ExpectCountUnmet (count != 9999)`), and add a line noting
*why* it is a positive-impossible count and not `0`: the CLI now rejects
`--expect-count 0` at parse time before the lock acquire and probe, which would
defeat both this baseline and the contention sentinel. Leave the
`--expect-count 1` invocation (~line 150) unchanged -- it already satisfies the
`N >= 1` guard and exercises a different (pool.json FIFO) ordering check.

## Change 4: document the `N >= 1` lower bound

In `docs/commands/discover.md`, the Flags table (~line 65) documents the flag
without a lower bound:

```
| `--expect-count <N>` | With `--write`, refuse to write if the discovered member count is not exactly `N` |
```

Update it to carry the contract, matching the CLI help and parse behavior:

```
| `--expect-count <N>` | With `--write`, refuse to write if the discovered member count is not exactly `N` (`N >= 1`; `--expect-count 0` is rejected at parse time) |
```

The "What happens under the hood" / "Safety checks" prose ("not exactly the
requested count") stays accurate and needs no change.

## Files

- `cli/src/main.rs` -- range guard on `DiscoverArgs::expect_count` (~line 438)
  and the new `discover_expect_count_zero_rejected` test (~line 1948).
- `tests/module/pool-lock-precedes-state-read.py` -- move the two
  `--expect-count 0` sentinels (~lines 112, 120) to `--expect-count 9999` and
  update the explanatory comment (~lines 107-111).
- `docs/commands/discover.md` -- note the `N >= 1` lower bound in the Flags
  table (~line 65).

## Verification

1. **Compiles (the crux of the pivot):** `cargo build` from `cli/` (or
   `just build`) succeeds -- confirms `RangedU64ValueParser::<usize>::new().range(1..)`
   type-checks against the `Option<usize>` field.
2. **Tests:** `just test-rust` (or `cargo test` in `cli/`) -- the new
   `discover_expect_count_zero_rejected` passes, and the existing
   `discover_write_with_expect_count_parses`, `discover_expect_count_without_write_rejected`,
   `discover_write_refuses_when_count_mismatches_{below,above}`, and
   `lock_deadline_zero_rejected` still pass (no regression from the unchanged
   `usize` pipeline).
3. **Manual sanity:**
   - `cargo run -- discover --write --expect-count 0` -> clap value-validation
     error on stderr, exit code 2 (a usage error, cleanly distinct from the
     exit-1 runtime fail-closed refusals).
   - `cargo run -- discover --write --expect-count 1` -> parses (then proceeds to
     the normal scan / preconditions; no parse error).
4. **VM sentinel (Change 3):** `just test-vm pool-lock-precedes-state-read`
   passes -- confirms the moved `--expect-count 9999` sentinels still exercise
   the empty-scan baseline and the lock-precedes-pending-op/probe ordering, and
   that the guard did not regress that coverage.
5. **Docs (Change 4):** `just docs-build` succeeds -- builds the mdBook and runs
   `mdbook-linkcheck2`, confirming the `discover.md` Flags edit is well-formed
   and breaks no links.
