# Plan: remove dead `Passphrase::len()` / `Passphrase::is_empty()`

## Context

`cli/src/secret.rs` defines `Passphrase`, the in-memory LUKS passphrase
boundary (a `Zeroizing<String>` newtype). Two of its public methods --
`len()` and `is_empty()` -- are dead code:

- A whole-repo caller search finds **no** invocation in `cli/src/` (source or
  unit tests), in `cli/tests/` integration tests (which compile against the
  `braid` lib as an external crate -- the one place a `pub` method could
  legitimately be consumed), or anywhere else. The only `.len()/.is_empty()`
  hits are the method bodies themselves.
- They were **born dead**: the introducing commit `179c2d7d`
  ("refactor(cli): introduce passphrase secret boundary") added both with the
  doc comment "used by validation tests" but added zero callers in the same
  diff. The doc was aspirational and wrong from day one.
- No dead-code warning fires because they are `pub` on `pub mod secret`, so
  rustc treats them as reachable library API even though nothing consumes the
  `braid` lib externally (single-member workspace; lib consumed only by its own
  bin). The rot is invisible.

The maintenance cost is a misleading doc plus two accessors on a
security-sensitive boundary that every reader must reason about (does `len()`
leak timing? is `is_empty()` a validation invariant?) when nothing depends on
them. Intended outcome: shrink the secret boundary to its real, used surface
(`from_zeroizing` + the single documented egress `expose_secret`).

## Change

Delete both methods from the `impl Passphrase` block in `cli/src/secret.rs#Passphrase`
-- the `len` and `is_empty` methods, their doc comments, and the blank separator
line between them. After deletion the `impl` block retains `from_zeroizing` and
`expose_secret`, both of which are used widely (`expose_secret` at
`cli/src/luks.rs` cryptsetup-stdin handoff sites; `from_zeroizing` at
read/finalize and many test constructors).

Resulting `impl Passphrase`:

```rust
impl Passphrase {
    /// Construct from an already-zeroizing read buffer without copying the
    /// plaintext into an intermediate unprotected owner.
    pub fn from_zeroizing(z: Zeroizing<String>) -> Self {
        Self(z)
    }

    /// Plaintext access for subprocess stdin handoff and narrow validation
    /// paths where the caller already owns the secret boundary.
    pub fn expose_secret(&self) -> &str {
        self.0.as_str()
    }
}
```

Nothing else changes. The `Debug` impl and the `passphrase_debug_redacts`
test are untouched.

## Why this is the ideal shape (and what NOT to do)

- **Delete both, not one.** Clippy's `len_without_is_empty` fires when a public
  `len()` exists without `is_empty()`. Removing the pair is clippy-clean;
  removing only one would trip the lint.
- **Do not replace with a test or stub.** Reintroduce `is_empty()` (or `len()`)
  only when a real validator needs it, with the caller in the same change.
  Adding a test now just to exercise the methods would re-justify dead code.
- **No ADR change required.** `docs/design/decisions/023-secret-handling.md`
  names only `expose_secret()` as the plaintext egress point; it never
  references `len()`/`is_empty()`. Deletion preserves the documented contract
  and strengthens its "single grep-friendly egress" story. (Verified: no doc,
  README, or plan references `Passphrase::len`/`Passphrase::is_empty`.)
- **Leave the sibling alone.** `LuksUuidMap::len()/is_empty()` in
  `cli/src/membership.rs` follows the same wrapper-newtype pattern but is
  genuinely used (`members.len()`, `membership.is_empty()` across
  discover/recover/preflight/main/journal). It is not in scope.

## Files

- `cli/src/secret.rs` -- the only file modified.

## Verification

1. `just test-rust` -- the selected Rust lane only
   (`cargo test --lib --bin braid --test golden_nixos_26_05 --test tty_guard
   --test confirm_yes`, then `test-state-modes`); it does **not** compile every
   `cli/tests` integration target. The relevant guard here is its `--lib`
   target, which builds the `secret` module and runs
   `secret::tests::passphrase_debug_redacts` -- confirming the lib still
   compiles and that test still passes after the deletion.
2. `just clippy` -- `cargo clippy --manifest-path cli/Cargo.toml --tests`. The
   `--tests` flag compiles **all** test targets, so this is the real
   all-target compile + lint guard: it confirms no caller anywhere (including
   the integration targets `test-rust` skips) depended on the methods, surfaces
   no new lint (in particular no `len_without_is_empty`), and no dead-code
   regression elsewhere.
3. Sanity grep -- `grep -n 'Passphrase::len\|Passphrase::is_empty\|\.len()\|\.is_empty()' cli/src/secret.rs`
   should return nothing: the only `.len()`/`.is_empty()` occurrences in the
   file were the deleted method bodies, so an empty result confirms both are
   gone. (Structural check only; the compile guard above does the real work.)
