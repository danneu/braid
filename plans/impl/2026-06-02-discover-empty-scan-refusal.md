# Plan: enrich the discover empty-scan refusal

## Context

A review finding flagged that `braid discover`'s empty-scan failure mode is
undocumented. Tracing it surfaced the real root cause: when the scan finds no
braid-labeled LUKS2 members, `cli/src/main.rs` (the `Commands::Discover` arm)
emits a bare `eprintln!("no braid-labeled LUKS devices found")` then exits 1 --
for **both** bare and `--write`.

That single line is the lone outlier among discover's refusals:

- It is the only discover *refusal* that bypasses `print_cli_error` (so it
  lacks the `error:` prefix every sibling refusal carries -- the other raw
  `eprintln!`s at `main.rs` lines ~956/~964 are success/usage *hints*, which
  correctly have no prefix).
- It carries **no remediation**, unlike every typed discover error
  (`DiscoverError::LabelCollision`, `::DuplicateUuid`,
  `DiscoverWriteError::ExpectCountUnmet`, `BareDiscoverError::*`), which all use
  the ` -- <fix>, then retry` house style.
- It has **no typed home** and **no test pinning its content**: the exact
  string appears only as a *negative* lock-ordering sentinel in
  `tests/module/pool-lock-precedes-state-read.py:81` (`assert "..." not in
  out`), which guards the message's *absence* under lock contention, not its
  wording -- so the remediation can silently regress, and (worse) renaming the
  string silently neuters that sentinel unless it is updated in lockstep
  (Change 4).

The operator hitting this is mid-recovery (rebuilding `pool.json` with a disk
momentarily detached, mislabeled, or LUKS1-only). A terse, prefix-less "found
nothing" tells them neither whether the command failed nor what to check --
contradicting braid's reason for existing (AGENTS.md: less error-prone, no
manpage reading). The fix is to make the message self-explanatory at its source
and bring it into the established convention, then close the doc gap.

**Intended outcome:** the empty-scan refusal becomes a typed,
remediation-bearing error surfaced like every other discover refusal, covered
by a regression test, and documented.

## Scope decision (why this shape, not the alternatives)

The empty-scan gate is a **shared caller policy**: it governs both bare preview
(which never writes) and `--write`. Per braid's mutation-safety heuristics,
*caller policy gates belong at callsites* -- so the gate stays at its current
single chokepoint in `main.rs`, before the bare/`--write` branch. We do **not**
add a second empty-guard inside `write_discovered_membership`: that would
duplicate the gate across two layers, and braid's fail-closed rule reserves
mandatory hardening for branches that "can corrupt state or strand a journal."
An empty `pool.json` is neither -- it is a self-evident 0-member state, already
prevented at the callsite, recoverable by re-running with disks attached. Only
the *message* needs a typed home, not the one-line `is_empty()` predicate.

We also keep the empty case **out of `DiscoverError`**. `discover_from_dir`
deliberately models "scanned successfully, found nothing" as
`Ok(PoolMembership::empty())` (the NotFound-dir arm in `discover.rs`, and scan
tests that assert `members.is_empty()` on an `Ok`). Folding empty into
`DiscoverError` to reuse the already-wired `drain_warnings -> print_cli_error`
path would conflate "found nothing" with "structural scan error," break those
tests, and push a caller-policy decision into the scan primitive. The empty
result stays `Ok`; the *refusal* is applied at the callsite.

## Changes

### 1. Typed error in `cli/src/discover.rs`

Add a unit-struct `thiserror` error alongside the other discover error families
(near `BareDiscoverError` / `DiscoverWriteError`). Unit struct, not a
single-variant enum: the message is static with no payload. Include the
required `///` doc comment justifying the boundary.

```rust
/// Shared post-scan refusal when discover finds zero braid-labeled LUKS2
/// members. Typed (not a bare `eprintln!`) so both the bare preview and
/// `--write` paths surface one remediation-bearing message through
/// `print_cli_error`, matching the other discover refusals.
#[derive(Debug, thiserror::Error)]
#[error(
    "no braid-labeled LUKS2 devices found -- check that pool members are attached and readable, and labeled braid-<name> as LUKS2 (LUKS1 or unreadable disks, if any, are skipped with a warning above)"
)]
pub struct NoMembersDiscovered;
```

Notes on wording: uses ` -- ` (no em-dash, per CLI output style); changes
today's "LUKS devices" -> "LUKS2 devices" (braid requires LUKS2; LUKS1 is
skipped). No backwards-compat concern (AGENTS.md). The rename is **not** free:
the old string is a lock-ordering sentinel in
`tests/module/pool-lock-precedes-state-read.py` and must be updated in lockstep
(Change 4), or the rename silently neuters it. The parenthetical is accurate:
warnings for LUKS1 (`UnsupportedLuksVersion`) and unreadable (`LuksDumpFailed`)
disks are drained by `drain_warnings` *above* this line.

### 2. Callsite in `cli/src/main.rs` (`Commands::Discover` arm)

Replace the bare `eprintln!` with the typed error routed through
`print_cli_error`, keeping the `is_empty()` check and `exit(1)` exactly where
they are (before the bare/`--write` branch, so both paths share it):

```rust
if members.is_empty() {
    print_cli_error(&braid_cli::discover::NoMembersDiscovered.to_string());
    std::process::exit(1);
}
```

This gives the message the `error:` prefix every sibling refusal already has.

### 3. Regression test in `cli/src/discover.rs` tests

Add a message-pin unit test mirroring the existing `DuplicateUuid` /
`ExpectCountUnmet` style (`err.to_string().contains(...)`), with the
`/* Intent / Why it exists / Scenario */` preamble. Assert on stable,
structure-insensitive substrings of the operator-facing contract, not the whole
string:

```rust
#[test]
fn no_members_discovered_message_carries_remediation() {
    /*
     * Intent: the empty-scan refusal names the LUKS2 requirement and the
     *   "attached and readable" remediation, in the discover " -- " house
     *   style, so it cannot silently regress to a bare "found nothing".
     * Why it exists: this message was previously a remediation-free
     *   eprintln! in main.rs that an operator rebuilding pool.json with a
     *   detached/mislabeled/LUKS1-only disk could not act on.
     * Scenario: operator runs `braid discover` with the array's disks
     *   momentarily detached and must learn what to check.
     */
    let msg = NoMembersDiscovered.to_string();
    assert!(msg.contains("no braid-labeled LUKS2 devices found"), "got: {msg}");
    assert!(msg.contains("attached and readable"), "got: {msg}");
    assert!(msg.contains("LUKS1"), "got: {msg}");
    assert!(msg.contains(" -- "), "got: {msg}");
}
```

(The existing `members.is_empty()` scan tests -- parse-failure and
dangling-symlink -- already cover *producing* an empty scan; this test covers
the *refusal message contract*, which nothing pins today.)

### 4. Update the lock-ordering sentinel in `tests/module/pool-lock-precedes-state-read.py`

The empty-scan message is referenced at line 81 as a *negative* sentinel that
proves `discover --write` fails on lock contention **before** it probes devices
(ADR 018 / principle 12 -- lock precedes any state read or probe):

```python
assert "no braid-labeled LUKS devices found" not in out, (
    "discover probed devices before acquiring lock; out=" + out
)
```

Renaming the message without updating this assertion does not break it -- it
makes it **trivially true forever** (the new string does not contain the old
substring), silently retiring the guard. Update it to the new lead clause, and
add a coupling comment so the next message edit touches both sides:

```python
# Lead clause must track NoMembersDiscovered's message in
# cli/src/discover.rs; a stale string here passes silently (ADR 018).
assert "no braid-labeled LUKS2 devices found" not in out, (
    "discover probed devices before acquiring lock; out=" + out
)
```

Match only the lead clause, not the remediation tail, so future wording edits
to the remediation don't disturb this sentinel.

### 5. Docs: one bullet in `docs/commands/discover.md`

Add a single bullet to **Safety checks**, placed right after the existing
dangling-symlink (`discover.md:81`) and LUKS1 (`discover.md:82`) skip bullets so
cause sits next to effect:

> - If no braid-labeled LUKS2 devices are found, `discover` exits 1 with
>   `no braid-labeled LUKS2 devices found -- ...` (both bare and `--write`) --
>   check the intended members are attached, readable, and labeled
>   `braid-<name>` as LUKS2. An array that is entirely LUKS1, detached, or
>   unreadable lands here, with any present-but-skipped disk warned about above.

## Explicitly out of scope

- **No write-layer empty guard** (see Scope decision).
- **Success/usage hints stay raw `eprintln!`** (`main.rs` ~956/~964): they are
  not errors and must not get the `error:` prefix.
- **No VM/integration test positively exercising the empty-scan exit.** No cheap
  host emits the empty-scan path without holding the pool lock, and every sibling
  refusal is tested only at the lib level, so a dedicated VM test would be
  disproportionate. The automated tests cover the message *string + remediation*
  (new lib test) and empty-scan *production* (existing scan tests) -- **not** the
  callsite's `print_cli_error` prefix routing, which stays unguarded like its
  siblings. (The `pool-lock-precedes-state-read` sentinel in Change 4 asserts the
  message's *absence* under contention; it does not exercise the print path.)
- **`README.md` unchanged** -- discover has only a one-line table entry there
  (`README.md` ~177), nothing that describes refusals.

## Critical files

- `cli/src/discover.rs` -- add `NoMembersDiscovered` (near the other error
  families) + the regression test (in the `#[cfg(test)] mod tests`).
- `cli/src/main.rs` -- swap the `eprintln!` for `print_cli_error(...)` in the
  `Commands::Discover` arm.
- `tests/module/pool-lock-precedes-state-read.py` -- update the line-81
  lock-ordering sentinel to the new message lead clause (+ a coupling comment).
- `docs/commands/discover.md` -- one Safety-checks bullet.

## Verification

1. `just test-rust` -- compiles the CLI (confirms the `main.rs` reference to
   `braid_cli::discover::NoMembersDiscovered`) and runs the new message-pin test
   plus the existing discover suite. (Crate package is `braid-cli`.)
2. `just test-vm pool-lock-precedes-state-read` -- confirms the updated
   lock-ordering sentinel still passes and remains a live assertion against the
   new message (not a trivially-true one).
3. `mdbook build docs` -- confirms the new Safety-checks bullet passes
   `mdbook-linkcheck2` (it adds no new cross-links, so this is a low-risk
   confidence check).
4. Real behavior (cannot be exercised in the dev sandbox; logic is covered by
   tests): on a NAS with no braid-labeled LUKS2 disks attached,
   `braid discover` prints
   `error: no braid-labeled LUKS2 devices found -- ...` and exits 1, for both
   bare and `--write`.
