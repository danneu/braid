# Enforce `discover --expect-count` requires `--write` at parse time

## Context

`braid discover --expect-count N` is a safety guard: it fails closed unless
discovery produces exactly `N` members, so a momentarily detached disk (loose
cable, USB power glitch, udev race) or a stray braid-labeled disk cannot
silently produce the wrong `pool.json` during a rebuild.

Today the guard is honored only inside the `if args.write` branch
(`cli/src/main.rs:919-923`); `args.expect_count` has exactly one consumer and
it is gated on `--write`. So `braid discover --expect-count 3` (operator forgot
`--write`) runs a read-only preview and silently drops the count guard. The
operator believes they ran a guarded rebuild. The arg's own doc comment already
claims "Only honored alongside --write" and the docs say "With `--write`, ..."
(`docs/commands/discover.md:51,73`), but nothing in the parser enforces the
coupling.

The fix makes clap reject the misuse at parse time instead of ignoring it,
bringing the parser in line with the documented contract. This is the same
mechanism `LockArgs` already uses for the `--systemd-stop` / `--deadline-secs`
coupling (`cli/src/main.rs:243,246`), so the pattern is established.

This was verified against the pinned clap (4.6.1 / clap_builder 4.6.0): the
"SetTrue flag always has a default value" gotcha does **not** apply.
`matched_arg.rs:136-138` returns `false` from `check_explicit(IsPresent)` when
the value source is non-explicit, and an absent `--write` carries
`ValueSource::DefaultValue`. Tracing `gather_requires` / `validate_required`
(`validator.rs:196-272`): passing `--expect-count` alone inserts `write` into
the required set, the missing-filter keeps it (absent => non-explicit), and the
parse fails with exit 2. Because `expect_count` is an `Option` with no default,
the requirement fires only when `--expect-count` is explicitly passed -- bare
`discover` and `discover --write` are unaffected.

## Changes

### 1. Add the clap constraint -- `cli/src/main.rs:412`

In `DiscoverArgs`, add `requires = "write"` to the `expect_count` arg:

```rust
#[arg(long = "expect-count", value_name = "N", requires = "write")]
expect_count: Option<usize>,
```

(`"write"` is the arg id derived from the `write: bool` field name on the same
struct.)

### 2. Tighten the doc comment -- `cli/src/main.rs:411`

Replace the now-stale "Only honored alongside --write." (which implies the arg
is parsed-but-ignored) with the enforced contract:

```rust
/// Requires --write.
```

The rest of the doc comment (lines 405-410) stays as-is.

### 3. Parser tests -- in-module, beside the `LockArgs` parse tests

Add **both** a positive and a negative test to the existing
`#[cfg(test)] mod tests` block (`cli/src/main.rs:1274`), immediately after the
`lock_*` parse tests (after `lock_deadline_zero_rejected`, ~line 1770). This
mirrors the closest precedent: `LockArgs` tests the same `requires` mechanism
with a colocated positive/negative pair using `Cli::try_parse_from` -- see
`lock_systemd_stop_with_deadline_parses` (main.rs:1740) and
`lock_systemd_stop_without_deadline_rejected` (main.rs:1752). `ErrorKind` is
already in scope via `use super::*`, and the variant is
`Commands::Discover(DiscoverArgs)` (main.rs:79).

These in-module parse tests are preferred over a spawned-binary test in
`root_check.rs`: they are faster (no process spawn) and the negative case
asserts on `ErrorKind` directly, so it is independent of clap's exact stderr
wording. The generic "clap usage errors exit 2 before the root gate" fact is
already covered by `add_requires_at_least_one_disk` and need not be re-proven.

**Positive -- locks the advertised guarded-write workflow (the review finding):**

```rust
// Intent: the documented guarded rebuild `discover --write --expect-count N`
//   still parses cleanly, with both flags reaching DiscoverArgs.
// Why it exists: the requires="write" constraint or a misnamed arg id could
//   over-tighten and break the only path that consumes expect_count (the
//   --write branch at main.rs:920); this is the advertised workflow in
//   docs/commands/discover.md.
// Scenario: operator runs `sudo braid discover --write --expect-count 3` to
//   guard a known 3-disk rebuild.
#[test]
fn discover_write_with_expect_count_parses() {
    let cli = Cli::try_parse_from(["braid", "discover", "--write", "--expect-count", "3"])
        .expect("guarded-write discover parses");
    let Commands::Discover(args) = cli.command else {
        panic!("expected discover command");
    };
    assert!(args.write);
    assert_eq!(args.expect_count, Some(3));
}
```

**Negative -- the misuse case:**

```rust
// Intent: `discover --expect-count N` without `--write` is rejected at parse
//   time instead of silently running a read-only preview.
// Why it exists: expect_count is a fail-closed guard honored only on the
//   --write branch; without requires="write" it is silently dropped and the
//   operator believes they ran a guarded rebuild when they did not.
// Scenario: operator runs `sudo braid discover --expect-count 3`, forgetting
//   --write, during a rebuild.
#[test]
fn discover_expect_count_without_write_rejected() {
    let err = Cli::try_parse_from(["braid", "discover", "--expect-count", "3"])
        .expect_err("expect-count requires write");
    assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
}
```

Note: the immediate `lock_*` siblings omit the Intent / Why / Scenario
preamble, but AGENTS.md requires it for new tests; keep the preambles short as
above.

## Non-goals / scope

- **Do not** make `--expect-count` work in read-only mode. It is a write-path
  guard by design (the doc comment and `docs/commands/discover.md` scope it to
  `--write` rebuilds); read-only `discover` is just a preview with nothing to
  guard. Failing closed at parse time is the intent-preserving fix.
- **No docs change needed.** `docs/commands/discover.md:51,73` and
  `docs/internals/luks-unlock.md:167` already describe the coupling as
  "With `--write`, ..."; the code is being brought in line with them.
- **No sibling refactor.** A grep of `main.rs` for other documented flag
  couplings ("only honored / alongside / ignored unless") found `expect_count`
  as the only instance, so there is no recurring pattern to extract.

## Verification

- `just test-rust` -- runs `cargo test`, which runs the in-module `mod tests`
  parse tests including the new `discover_write_with_expect_count_parses` and
  `discover_expect_count_without_write_rejected`. This is the primary gate.
- Manual sanity (optional):
  - `cargo run -p braid-cli -- discover --expect-count 3` => exits 2, usage
    error naming `--write`.
  - `cargo run -p braid-cli -- discover --help` => help still renders;
    `--expect-count` shown.
  - `cargo run -p braid-cli -- discover --write --expect-count 3` => parses past
    clap (then hits the normal root/runtime path), confirming the valid
    combination is unaffected.
- No NixOS VM tests are needed: this is a pure CLI-parse change with no systemd,
  mount, pool-lock, or parser blast radius.
