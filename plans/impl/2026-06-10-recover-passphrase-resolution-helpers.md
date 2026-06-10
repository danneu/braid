# Plan: collapse the recover passphrase-resolution helpers

## Context

`cli/src/recover.rs` carries three passphrase-resolution helpers that
overlap:

- `recover_passphrase` (add path) -- resolves `Option<&OpenCredential>` to a
  `RecoverPassphrase`; key-file arm errors with the literal
  `"add recovery requires a passphrase for delayed LUKS format"`.
- `recover_passphrase_for_context` (replace path) -- byte-for-byte the same
  resolver, except the key-file arm errors with `"{context} requires a
  passphrase"`. Introduced in `e14ee8f5` as a copy of the above with a
  `context` param.
- `open_credential_passphrase` (pre-mount discovery) -- a *tighter* helper:
  takes `&OpenCredential` (not `Option`), returns a borrowed `&Passphrase`
  (not `RecoverPassphrase`), and has **no prompt arm**, so it structurally
  cannot read a fresh passphrase. Key-file arm: `"{context} requires a
  passphrase"`.

The key-file rejection -- the single policy "recover refuses a key-file
credential; it has not exposed `--key-file` yet"
(`cli/src/recover.rs#open_credential_passphrase` and
`cli/src/recover.rs#recover_passphrase`) --
is duplicated across all three. A future `recover --key-file` would have to
change it in three places and is easy to get inconsistent.

This is a code-review finding (Low / Simplicity). The finding proposed
deleting `recover_passphrase` and folding `open_credential_passphrase` away.
This plan **pivots**: the two true duplicates merge, but
`open_credential_passphrase` is *kept as the single rejection site* and the
merged resolver **delegates** its borrowed arm to it -- so the policy lives in
exactly one place (better than the finding's plan, which would leave it
duplicated across two helpers). `open_credential_passphrase` also encodes a
real invariant at its call site (cannot prompt, borrowed-only) that folding it
away would discard.

Outcome: **2 helpers instead of 3, key-file policy in 1 place instead of 3**,
no change to any reachable behavior.

## Approach

### 1. Merge the two duplicate resolvers, delegate the rejection

Delete `recover_passphrase`. Rename `recover_passphrase_for_context` ->
`recover_passphrase` (the `_for_context` suffix only existed to disambiguate
from the now-deleted literal variant; comments in
`cli/src/recover.rs#execute_recover_initial_open` and
`cli/src/recover.rs#tests` already refer to the concept as
`recover_passphrase`, so the rename re-aligns them). Change its `Some` arm to
delegate to `open_credential_passphrase` so the key-file refusal is no longer
inlined.

Co-locate the survivor next to `open_credential_passphrase` (the two
credential->passphrase helpers belong together) and remove the old definition
site. Final shape of the two helpers:

```rust
/// The single place recover refuses a key-file credential. Returns the
/// borrowed passphrase, or fails with "{context} requires a passphrase" --
/// recover does not expose `--key-file` today, so a future `recover --key-file`
/// changes this policy in exactly one spot. Both the pre-mount discovery path
/// (which already holds a resolved credential) and `recover_passphrase`'s
/// borrowed arm funnel through here.
fn open_credential_passphrase<'a>(
    credential: &'a OpenCredential,
    context: &str,
) -> Result<&'a Passphrase, RecoverError> {
    match credential {
        OpenCredential::Passphrase(passphrase) => Ok(passphrase),
        OpenCredential::KeyFile(_) => Err(RecoverError::Failed(format!(
            "{context} requires a passphrase"
        ))),
    }
}

/// Resolve the passphrase recovery drives `cryptsetup` with: borrow it from an
/// already-open credential, or read it fresh when none was resolved yet.
/// `context` names the operation ("add recovery" / "replace recovery") in the
/// key-file rejection, which is delegated to `open_credential_passphrase` so
/// the refusal lives in one place.
fn recover_passphrase<'a>(
    existing: Option<&'a OpenCredential>,
    params: &RecoverParams<'_>,
    context: &str,
) -> Result<RecoverPassphrase<'a>, RecoverError> {
    match existing {
        Some(credential) => Ok(RecoverPassphrase::Borrowed(open_credential_passphrase(
            credential, context,
        )?)),
        None => Ok(RecoverPassphrase::Owned(luks::read_passphrase_with(
            params.passphrase_file,
            params.passphrase_stdin,
            false,
            params.tty,
        )?)),
    }
}
```

Lifetimes check: `existing: Option<&'a OpenCredential>` yields `credential:
&'a OpenCredential`; `open_credential_passphrase` returns `&'a Passphrase`;
`RecoverPassphrase::Borrowed(&'a Passphrase)` is `RecoverPassphrase<'a>`. The
borrow checker validates the delegation -- this is a compile-checked refactor.

### 2. Update the four call sites

All four pass the same local `credential` and gain/keep a `context` arg:

- Add path, LUKS-open site in
  `cli/src/recover.rs#execute_add_pool_mutation_recovery` ->
  `recover_passphrase(credential, params, "add recovery")`
- Add path, replay-verify site in
  `cli/src/recover.rs#execute_add_pool_mutation_recovery` ->
  `recover_passphrase(credential, params, "add recovery")`
- Replace path in `cli/src/recover.rs#finish_uncommitted_replace_recovery` ->
  rename only:
  `recover_passphrase(credential, params, "replace recovery")`

`open_credential_passphrase`'s own call site
(`cli/src/recover.rs#discover_add_targets_before_mount`, context
`"add recovery pre-mount discovery"`) is unchanged.

### 3. Error-wording decision: drop the "delayed LUKS format" rationale

The unified `"{context} requires a passphrase"` template cannot reproduce the
add path's current `"add recovery requires a passphrase for delayed LUKS
format"` verbatim (its rationale lands after the word "passphrase"). **Drop the
rationale** -- the messages become the symmetric pair:

- `"add recovery requires a passphrase"`
- `"replace recovery requires a passphrase"`

This is correct for braid, not just simpler:

- The key-file arm is a **fail-closed guard unreachable via today's CLI**
  (recover passes `key_file = None`, so `resolve_credential` only ever yields
  `OpenCredential::Passphrase`). Its audience is a future dev, not an operator
  mid-recovery; a generic message reads clearer than a sub-path-specific one.
- The rationale is **already inaccurate at one of its two current add sites**:
  the membership-**replay** path in
  `cli/src/recover.rs#execute_add_pool_mutation_recovery` is not a delayed
  format. Dropping it makes the message right at both sites.
- The `context: &str` param leaves the hook for a future `recover --key-file`
  to reintroduce precise per-flow messaging if the arm ever becomes reachable.

No test pins these strings and no doc quotes them (`grep` for `"for delayed
LUKS format"` and `"requires a passphrase"` over `cli/`, `tests/`, `docs/`
hits only the source defs), so the change is observable nowhere reachable.

### 4. Doc comments

Both survivors get `///` intent comments (shown above) per the AGENTS.md
doc-comment convention -- in particular naming `open_credential_passphrase` as
*the* single key-file rejection site. (The three originals carry no `///`
today, contrary to the finding's description.)

### 5. Lock the consolidated policy with one unit test (recommended)

Because the rejection arm is unreachable via the CLI, a unit test is the only
way to pin it behaviorally -- which makes it the sole guard for the invariant
the finding worried about. Add to `mod tests` (`cli/src/recover.rs#tests`). It
carries the literal `// Intent` / `// Why it exists` / `// Scenario` preamble
required of every test (`docs/dev/testing.md#conventions`; the example there is
a Rust `#[test]`, and `recover.rs` already follows it):

```rust
// Intent: a key-file credential reaching recover's passphrase boundary is
//   refused with a "requires a passphrase" error, never silently accepted.
// Why it exists: this OpenCredential::KeyFile arm is a fail-closed guard on a
//   branch unreachable through today's CLI (recover exposes no --key-file), so
//   a unit test is its only behavioral guard -- and it locks the single
//   post-refactor rejection site so a future `recover --key-file` cannot
//   reintroduce the three-places-to-change hazard this pivot removed.
// Scenario: a contributor wires `recover --key-file` (or an internal path hands
//   recover a resolved key-file credential) and routes it into add/replace
//   recovery; the guard must still fire with the passphrase hint, not proceed.
#[test]
fn open_credential_passphrase_rejects_key_file() {
    let cred = OpenCredential::KeyFile(std::path::PathBuf::from("/dev/null"));
    let err = open_credential_passphrase(&cred, "add recovery").unwrap_err();
    assert!(
        matches!(err, RecoverError::Failed(msg) if msg.contains("requires a passphrase")),
        "key-file credential must be refused with a passphrase hint, got {err:?}"
    );
}
```

Behavioral (asserts the refusal policy + hint) and structure-insensitive (does
not care that the resolver now delegates here). The test module is in-file, so
it can call the private helper directly.

## Files

- `cli/src/recover.rs` -- the only file changed. Delete one helper, rename +
  co-locate + delegate another, keep the third, update four call sites, refresh
  two doc comments, add one unit test.

## Verification

- `cargo build` (or `just build`) -- the borrow checker validates the
  delegated lifetime; the exhaustive `match` on `OpenCredential` /
  `Option<&OpenCredential>` validates the merge. A successful build is most of
  the proof for a refactor this mechanical.
- **Structural outcome -- proves the de-duplication, not just that it
  compiles.** `cargo build` and the unit test pass even if a stray duplicate,
  the old string, or a second rejection arm survives, so assert the collapse
  directly over `cli/src/recover.rs` (`rg`):
  - `rg -c 'fn recover_passphrase\b' cli/src/recover.rs` -> `1` (one merged
    resolver; `\b` excludes `recover_passphrase_for_context`).
  - `rg -n 'recover_passphrase_for_context' cli/src/recover.rs` -> no matches
    (duplicate resolver deleted, both replace call sites renamed).
  - `rg -c 'fn open_credential_passphrase' cli/src/recover.rs` -> `1` (the
    no-prompt rejection helper is kept, not folded away).
  - `rg -c 'OpenCredential::KeyFile\(_\)\)? => Err\(RecoverError::Failed' cli/src/recover.rs`
    -> `1` (one key-file rejection match arm -- the bare arm in
    `open_credential_passphrase`; the `\)?` also matches a stray
    `Some(OpenCredential::KeyFile(_))` arm, so a half-merged resolver fails this
    gate too: 3 matches before the refactor, 1 after). A `requires a passphrase`
    substring gate would false-fail here -- the new doc comment and the unit
    test contain that phrase by design.
  - `rg -n 'delayed LUKS format' cli/src/recover.rs` -> no matches (the old
    add-path wording is retired).
- `just test-rust` (or `cargo test -p braid recover`) -- runs the in-file
  `mod tests`, including the new `open_credential_passphrase_rejects_key_file`.
  No existing test references the three helpers, so the rest must pass
  unchanged.
- `python3 scripts/docs/check-output-ascii.py` -- the new message strings are
  ASCII; confirms the output-ASCII convention still holds over `cli/src`.
- Existing recover add/replace NixOS VM tests under `tests/` are the
  integration safety net but need not be run for this change: no reachable
  behavior is altered, and the unit + compile checks cover the merge.

## Implementation notes

- The unit test was named `key_file_credential_is_rejected_at_recover_passphrase_boundary`
  instead of the draft's `open_credential_passphrase_rejects_key_file` so the
  plan's `rg -c 'fn open_credential_passphrase' cli/src/recover.rs` structural
  gate still proves there is only one helper definition.
