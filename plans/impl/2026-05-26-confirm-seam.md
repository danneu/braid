# Confirm seam: make the `!yes` gate testable across all four mutating commands

## Context

A `/ultrareview` finding (Low / Testing) noted that the interactive
confirmation path in `remove-missing` is untested: `format_remove_missing_confirm`
is only unit-tested in isolation, and every command/VM test passes `--yes`, so
the `if !params.yes { ... }` branch in `RemoveMissingPlan::execute`
(`cli/src/remove_missing.rs:172-183`) is never exercised end-to-end.

Investigation (`/verify-issue`) found the gap is real but the finding's proposed
test cannot be written as-is, and the gap is **not** remove-missing-specific:

- There is **no confirm seam**. All four mutating commands (`add.rs:1001`,
  `remove.rs:278`, `remove_missing.rs:182`, `replace.rs:477`) call the no-arg
  `crate::confirm::confirm_yes()`, which `dup`s the real process stdin. A unit
  test driving `yes=false` would block on a tty or read EOF nondeterministically.
- The load-bearing untested invariant is **ordering**: confirm must run *before*
  the sleep-inhibitor acquire and the journal write (documented at
  `remove_missing.rs:192-196`, `remove.rs:288`, `replace.rs:561`). Existing tests
  assert `inhibitor.acquire_count() == 0` only for *preflight* refusals; a
  regression moving the confirm gate after the inhibitor/journal (stranding
  `pending-op.json` on a decline) passes every current test.
- The specific "swapped `remaining_present`/`missing_count`" regression the
  finding named is statically impossible (`usize` vs `u64`), but same-typed
  mis-wiring (`missing_id`/`missing_count`, both `u64`) and a dropped gate are
  real and uncaught.

**Outcome:** add a shared `Confirm` dependency-injection seam mirroring the
existing `AcquireSleepInhibitor` seam, route every command's confirmation prompt
through it, and pin the gate + the confirm-before-side-effects ordering + the
prompt wiring with unit tests. Per the chosen scope, **all four** commands adopt
the seam. `add` adopts it by adding the new `confirm` field to its existing
`AddParams` literals -- **not** via an `AddParamsBuilder` migration, which would
be broader than the seam needs and risks silently changing the varied local
state (paths, seeded/fresh membership, local inhibitors, passphrase files) those
inline tests set up.

## Design

### The seam (template: `cli/src/inhibit.rs:52-97`)

The seam copies the inhibitor pattern exactly: a trait, a zero-sized production
impl wrapping the existing primitive, and a `#[cfg(test)]` recording impl with
interior mutability that tests inspect after the run.

Wide seam (prompt-passing) so tests can assert the prompt body, not just the
verdict. Each command assembles its complete prompt string (formatter output
plus any trailing `WARNING:` line, byte-for-byte as today) and hands it to the
seam; the real impl prints it then reads "yes".

In `cli/src/confirm.rs` (reuse existing `confirm_yes()` — do not duplicate stdin
logic):

```rust
/// Seam for the operator go/no-go prompt so command tests can drive the
/// `!yes` gate without a real tty. Production prints the assembled prompt and
/// reads "yes" from stdin; tests record the prompt and return a canned verdict.
pub trait Confirm {
    fn confirm(&self, prompt: &str) -> Result<(), String>;
}

/// Production confirm: print the assembled prompt, then require "yes" from the
/// real tty via `confirm_yes`.
pub struct RealConfirm;
impl Confirm for RealConfirm {
    fn confirm(&self, prompt: &str) -> Result<(), String> {
        eprint!("{prompt}");        // command owns exact bytes incl. trailing \n
        confirm_yes()
    }
}

#[cfg(test)]
pub struct RecordingConfirm {
    verdict: std::cell::Cell<Verdict>,        // Unexpected (default) | Accept | Decline
    prompts: std::cell::RefCell<Vec<String>>,
}
// Fail-closed default: new() -> Verdict::Unexpected, and `impl Confirm` records
// the prompt then PANICS if the verdict was never armed. So a regression that
// prompts even when `--yes` is true fails the test instead of silently passing
// (an accepting default would mask it). accept(&self)/decline(&self) arm the
// verdict; only `yes=false` tests call them. last_prompt()/prompts() expose the
// recorded prompts. Armed verdicts: Accept -> Ok(()); Decline ->
// Err("aborted by user") (matches confirm_yes's decline string).
```

All new `pub`/`pub(crate)` items need `///` doc comments per AGENTS.md.

### Thread the seam through the four commands

For each of `add.rs`, `remove.rs`, `remove_missing.rs`, `replace.rs`:

1. Add `pub confirm: &'a dyn Confirm` to the params struct (alongside
   `sleep_inhibitor`): `AddParams` (`add.rs:843`), `RemoveParams` (`remove.rs:86`),
   `RemoveMissingParams` (`remove_missing.rs:63`), `ReplaceParams` (`replace.rs:151`).
2. Rewrite the `if !params.yes { ... }` block: build the full prompt `String`
   and replace the direct `eprintln!(prompt)` + `confirm::confirm_yes()` with
   `params.confirm.confirm(&prompt).map_err(Error::Validation)?;`. Because
   `RealConfirm` prints with `eprint!` (no added newline), the assembled prompt
   must reproduce the old `eprintln!` bytes exactly -- **append `\n` to the
   formatter result**: `format!("{}\n", formatter(...))`. (`replace` already does
   this via `format!("{}\n", format_replace_confirm(...))`.)
   - `remove.rs`/`replace.rs` then append their 1-disk `WARNING:` bytes exactly as
     today -- each ends in **two** newlines (`...\n\n`), because the warning
     string carried a trailing `\n` and the old `eprintln!`/emit added another:
     remove appends `"WARNING: Pool will have 1 disk -- no RAID1 redundancy.\n\n"`,
     replace appends `"WARNING: This replace leaves only 1 disk -- no redundancy.\n\n"`.
   - `replace.rs`: the confirm prompt stops going through `emit_replace_stderr`;
     that helper stays for replace's *other* stderr (status/probe/post-op). Safe:
     no test asserts the confirm prompt via `replace_stderr_capture` (it covers
     lines like "post-replace probe failed", `replace.rs:4844`); the only
     `.yes(false)` tests are dry-run (`replace.rs:5583,5656`) and never reach the
     gate.

### Production wiring (`cli/src/main.rs`)

Hoist once next to `let sleep_inhibitor = ...RealSleepInhibitor;` (`main.rs:498`):

```rust
let confirm = braid_cli::confirm::RealConfirm;
```

Add `confirm: &confirm,` to all four params literals (`main.rs:533` add, `571`
remove, `602` remove_missing, `640` replace).

### Fixtures + builders

- `PoolFixture` (struct def `test_fixtures/shared.rs:289`): add
  `pub(crate) confirm: RecordingConfirm`. **Every** fixture-construction literal
  must initialize it -- there are **7 across 4 files**, not just shared.rs's 3.
  Inventory them with `rg "inhibitor: RecordingInhibitor::new\(\)" cli/src/test_fixtures`
  (today: `shared.rs` x3, `remove_missing.rs` x2, `replace.rs` x1, `remove.rs` x1)
  and add `confirm: RecordingConfirm::new()` (fail-closed) beside each `inhibitor:`
  line -- this is the reliable signal and avoids hard-coding drifting line
  numbers. Note `recover.rs`'s `impl PoolFixture` only adds a `recover_params()`
  builder method (no fixture literal, no `inhibitor:` init), so it needs no change.
  Builders default to `&self.confirm`; gate tests arm it with `f.confirm.accept()`
  / `f.confirm.decline()` before running the command.
- `RemoveParamsBuilder`, `RemoveMissingParamsBuilder`, `ReplaceParamsBuilder`
  (`test_fixtures/{remove,remove_missing,replace}.rs`): add
  `confirm: &'a RecordingConfirm` field, wire `confirm: self.confirm` in
  `build()`. **Add a `.yes(bool)` setter to `RemoveMissingParamsBuilder`** — it
  is the only one missing one (`remove`/`replace` already have it). Existing
  tests pass `yes:true`, never invoke `confirm`, and are otherwise unchanged.
- `add.rs` (**no `AddParamsBuilder`** -- field-add only): add
  `confirm: RecordingConfirm` to `PlanAddFixture` (`add.rs:8145`, init
  `RecordingConfirm::new()`). Add the `confirm` field to the central
  `PlanAddFixture::params_with_config` literal (`add.rs:8178`) once, defaulting
  to `&self.confirm` -- this covers every helper-based test. The standalone
  inline `AddParams { ... }` literals that don't use the helper (they thread
  local `paths`/`config`/`inhibitor`/`resolver` and sometimes seeded membership
  or acked-stats -- e.g. `add.rs:3043, 5290`) each get a local
  `let confirm = RecordingConfirm::new();` plus `confirm: &confirm,`. All these
  sites pass `yes: true`, so the fail-closed recorder is never called and their
  local state is left exactly as-is. For the new add confirm tests (which need
  `yes=false`), introduce only the minimal helper required -- a
  `yes`-parameterized variant of `params_with_config`, or an inline `yes=false`
  literal in the test -- not a general builder.

### Tests to add

All four commands get the same three-test shape; arm the recorder with
`f.confirm.decline()` / `f.confirm.accept()` (only these `yes=false` tests touch
it). Primary target is `remove_missing.rs` (3-disk-one-missing fixture):

1. **Declined confirm aborts before side effects** — `f.confirm.decline()`;
   expect `Err` whose message is "aborted by user";
   `f.inhibitor.acquire_count() == 0`; `journal::load_journal(&f.paths).is_none()`;
   no `BtrfsDeviceRemove` in `runner.requests()`. (Pins the ordering invariant.)
2. **Accepted-confirm prompt wiring** — `f.confirm.accept()`; assert
   `f.confirm.prompts()` is **exactly one** entry equal to the exact assembled
   prompt. Build `expected` the way the command does -- formatter result **plus
   the trailing `\n`** that replaced `eprintln!` (for remove-missing:
   `format!("{}\n", format_remove_missing_confirm(<name>, 3, 2, 1))`, no WARNING).
   Asserting the whole `prompts()` vec (length 1), not just `last_prompt()`, pins
   the formatter args, assembly, trailing newline, **and cardinality** -- catching
   a dropped / garbled / off-by-a-newline prompt, a same-typed (`u64`) mis-wiring,
   and a double-prompt regression -- without re-asserting the formatter's literal
   text (that stays covered by the isolation tests).
3. **Accepted confirm proceeds** — `f.confirm.accept()`; command completes and
   the mutating command (`BtrfsDeviceRemove` for remove-missing) is issued (the
   gate does not false-abort).

Sibling coverage -- `remove`, `replace`, and `add` each get the same three
tests, with their own mutating-command assertion in test 3 and the same
`prompts()`-length-1 exact-equality check in test 2. For test 2, `remove` and
`replace` must additionally cover the **1-disk topology** (remove:
`remaining == 1`; replace: `total_devices == 1`) so the `expected` prompt
includes the trailing `WARNING:` bytes (ending in two newlines, as above); a
non-warning case covers the normal shape. (`remove_missing` and `add` have no
post-prompt WARNING.)

Follow the test-preamble convention (Intent / Why it exists / Scenario) from
AGENTS.md / `docs/dev/testing.md`. The existing isolation formatter tests
(`remove_missing.rs:1948-2021`, `replace.rs:2104/2134/2453`) stay as-is.

## Critical files

- `cli/src/confirm.rs` — new `Confirm` trait, `RealConfirm`, `RecordingConfirm`.
- `cli/src/{add,remove,remove_missing,replace}.rs` — params field + gate rewrite
  + the three confirm tests in each (decline-ordering, accepted prompt-wiring,
  accepted-proceeds); remove/replace wiring tests also cover the 1-disk WARNING.
- `cli/src/main.rs` — hoist `RealConfirm`, wire four params literals.
- `cli/src/test_fixtures/shared.rs` — `PoolFixture.confirm`.
- `cli/src/test_fixtures/{remove,remove_missing,replace}.rs` — builder field +
  `build()`; add `.yes()` to remove_missing.
- `cli/src/add.rs` (test module) — `PlanAddFixture.confirm` (fail-closed);
  `confirm` field added to existing `AddParams` literals (central
  `params_with_config` once + each standalone literal with a local recorder);
  minimal `yes=false` helper for the new add confirm tests. No `AddParamsBuilder`.

## Verification

- `just test-rust` — runs the full Rust unit suite: the new `confirm.rs` seam
  tests and the three per-command confirm tests (decline-ordering, prompt-wiring,
  accepted-proceeds) for all four commands. This is the primary gate.
- During iteration, narrow with `cargo test -p braid-cli confirm` and per-module
  (`remove_missing`, `add`, `remove`, `replace`) runs.
- No VM tests required — this is a pure-Rust DI seam with no module/systemd
  surface. Do **not** run `cargo fmt` (AGENTS.md); keep edits hand-narrow.
- Sanity: `rg -n "confirm::confirm_yes\(\)" cli/src` should return only the
  `RealConfirm` impl in `confirm.rs` after the change (no command calls it
  directly anymore).

## Implementation notes

- The replace prompt-wiring test uses a synthesized one-disk `ReplacePlan::execute`
  to pin the `total_devices == 1` warning bytes; the command-level replace
  decline/proceed tests still exercise `cmd_replace` with the shared fixture.
