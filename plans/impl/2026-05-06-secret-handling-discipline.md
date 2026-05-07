# Plan: codify braid's secret-handling discipline

## Context

Two recent fixes hardened braid's in-process secret surface end-to-end:

- `cc95789 fix(cli): zeroize luks passphrase reads end-to-end` -- TTY,
  stdin, and file passphrase reads all flow through `Zeroizing` types.
  Byte-by-byte unbuffered reads avoid std-internal `BufRead` allocations.
- `0ec5636 fix(cli): zeroize generated keyfile buffer` -- 4096-byte
  keyfile buffer is `Zeroizing<[u8; KEYFILE_SIZE]>`, dropped before the
  durability fsync.

The resulting design is coherent but the rules live only in scattered
code+comments. This plan locks them in three ways:

1. Write the rules down as an architectural decision record so future
   contributors can be pointed at a contract during review.
2. Replace the loose `Zeroizing<String>` convention with a typed
   `Passphrase` newtype so the type system -- not human discipline --
   enforces the contract at every call site.
3. Keep the only in-process passphrase comparison
   (`check_passphrase_match`) scoped to local double-prompt
   confirmation. This is a typo guardrail before first-device
   formatting, not an authentication oracle.

The three pieces reinforce each other: the ADR describes the contract,
the newtype enforces the ownership boundary, and the confirmation
check remains ordinary local validation.

---

## Improvement 1 -- ADR `docs/decisions/023-secret-handling.md`

New file. Status: `Active`.

### Frontmatter and discoverability

- The ADR opens with the project's standard YAML frontmatter (per
  [`docs/index.md:7-17`](../../docs/index.md#frontmatter)):
  ```yaml
  ---
  intent: Required types and disciplines for in-process LUKS secret
    material. Read before modifying passphrase reads, the keyfile
    generator, the OpenCredential / Passphrase types, or the
    subprocess handoff in luks.rs.
  ---
  ```
- Add an entry under the `decisions/` heading in
  [`docs/index.md`](../../docs/index.md), after the existing 022
  bullet, in the same format as siblings:
  ```markdown
  - [decisions/023-secret-handling.md](decisions/023-secret-handling.md) -- **Active.** Required types and disciplines for in-process LUKS secret material: Zeroizing typing, no BufRead in passphrase paths, hard byte caps, subprocess stdin (never argv), drop-before-fsync for generated secrets, redacted Debug, and typed passphrase boundaries.
  ```
- Both edits land in commit 1 alongside the new ADR file.

### Rules to document

1. **In-memory secret typing.** Any value that contains LUKS passphrase
   plaintext or LUKS keyfile bytes must be `Zeroizing<T>` (or a newtype
   wrapping one), so `Drop` zeroes the heap pages. Naming convention:
   the dedicated newtype is `secret::Passphrase` (Improvement 2);
   keyfile byte buffers stay as `Zeroizing<[u8; KEYFILE_SIZE]>` because
   the bytes never leave the function frame.
2. **No `BufRead` in passphrase paths.** Passphrase TTY/stdin reads use
   unbuffered `Read` and consume one byte at a time into a pre-sized
   `Zeroizing` buffer. Cross-reference: `cli/src/luks.rs:295-314`
   (`read_line_into_zeroizing`), and the explicit comment at
   `cli/src/confirm.rs:115-116` ("intentionally accepts `Read`, not
   `BufRead`, so confirmation cannot pre-drain bytes needed by a later
   `--passphrase-stdin` read").
3. **Hard byte cap on every secret-bearing read.**
   `PASSPHRASE_MAX_BYTES = 64 * 1024` and `CONFIRM_MAX_BYTES = 256`
   are size limits enforced at read time so a hostile pipe cannot grow
   a `Zeroizing<Vec<u8>>` indefinitely. Any new secret-read site must
   declare and enforce a cap.
4. **Subprocess handoff is via stdin, never argv.** Anything inside a
   `Passphrase` reaches the child process through
   `CommandRunner::run_with_stdin`, never through `CmdRequest::to_argv`.
   `ps(1)` must never be able to surface a passphrase.
5. **Generated-secret early drop.** Generated random secrets must drop
   before any subsequent syscall whose duration is unbounded -- in
   particular, before the durability `fsync`/`sync_all` on the file
   they were just written to. Cross-reference:
   `cli/src/enroll_key_file.rs:327-340` (the inner block scope makes
   the `Zeroizing<[u8; KEYFILE_SIZE]>` drop before
   `f.sync_all()`).
6. **Redacted `Debug`.** Every type that owns secret bytes must
   implement `Debug` to render as `<redacted>` (or equivalent). See
   `cli/src/credential.rs:27-34` (`OpenCredential::Debug`).
7. **Passphrase comparison is not authentication.** braid delegates
   real passphrase verification to cryptsetup/LUKS. The only current
   in-process comparison is `check_passphrase_match`, used by the
   fresh-format double-prompt flow to catch local typos before the
   entered passphrase becomes canonical. This ordinary equality check
   stays local to that helper; `Passphrase` itself does not implement
   `PartialEq`.
8. **Threat model boundary.** These rules harden the in-process memory
   image (process snapshot, core dump, swap residue). They do NOT
   defend against a privileged attacker on the running host
   (`ptrace`, `/dev/mem`, root reading `/proc/<pid>/mem`). braid's
   threat model is "no plaintext beyond the smallest possible
   in-process window"; that is the rule the type system encodes.

### ADR cross-references

- `docs/decisions/004-single-passphrase.md` (UX policy that produced the
  surface).
- `docs/decisions/018-systemd-lifecycle.md` (one consumer of the
  `Passphrase` newtype is `braid unlock` running under
  systemd-ask-password).
- `cli/src/secret.rs` (the newtype, post-Improvement-2).

---

## Improvement 2 -- `Passphrase` newtype

### New module `cli/src/secret.rs`

```rust
use zeroize::Zeroizing;

/// In-memory LUKS passphrase. Wraps `Zeroizing<String>` so the secret
/// bytes are scrubbed on drop and the secret can only leave the type
/// via `expose_secret()` -- a grep-friendly call site that signals
/// "this is where plaintext crosses a process boundary."
///
/// Deliberately not `Clone`: every additional copy is another heap
/// region holding plaintext until its `Drop` runs. Production call
/// sites either move the value (read pipeline -> `OpenCredential`,
/// `RecoverPassphrase::Owned`) or borrow it (`&Passphrase` to verify
/// or subprocess-handoff helpers). Tests construct fresh values via
/// `from_zeroizing`.
pub struct Passphrase(Zeroizing<String>);

impl Passphrase {
    pub fn from_zeroizing(z: Zeroizing<String>) -> Self {
        Self(z)
    }

    /// Plaintext access for subprocess stdin handoff. Call sites
    /// using this method are the documented secret-egress points.
    pub fn expose_secret(&self) -> &str {
        self.0.as_str()
    }

    pub fn len(&self) -> usize { self.0.len() }
    pub fn is_empty(&self) -> bool { self.0.is_empty() }
}

impl std::fmt::Debug for Passphrase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Passphrase(<redacted>)")
    }
}
```

### Cargo

- No new dependency is needed; `Passphrase` only wraps the existing
  `zeroize` dependency.

### Wire it into `lib.rs`

- Add `pub mod secret;` to `cli/src/lib.rs`, with a `///` boundary
  doc comment immediately above it (per the project's
  doc-comment convention for top-level Rust items, and matching the
  style already used for `credential` at
  [`lib.rs:10-13`](../../cli/src/lib.rs#L10)). Suggested wording:
  ```rust
  /// In-memory secret types (currently `Passphrase`) that scrub on
  /// drop and gate plaintext egress through `expose_secret()`. Sibling
  /// to `credential` (resolved-credential containers) and
  /// `credential_verify` (borrowed-credential verification).
  pub mod secret;
  ```
  So other modules can `use crate::secret::Passphrase;`.

### Replace `Zeroizing<String>` returns in the read pipeline

`cli/src/luks.rs`:

| Site | Before | After |
| --- | --- | --- |
| `PassphraseReader::read_tty` (line 101) | `Result<Zeroizing<String>, LuksError>` | `Result<Passphrase, LuksError>` |
| `read_tty_from_file` (line 128) | same | same |
| `ScriptedPassphraseReader::read_tty` (line 211) | same | same |
| `read_passphrase` (line 224) | same | same |
| `read_passphrase_with` (line 244) | same | same |
| `read_passphrase_with_readers` (line 274) | same | same |
| `finalize_passphrase_bytes` (line 350) | `Result<Zeroizing<String>, LuksError>` | `Result<Passphrase, LuksError>` |
| `check_passphrase_match` (line 379) | takes/returns `Zeroizing<String>` | takes/returns `Passphrase`, compares `expose_secret()` strings for local confirmation only |

Implementation note: `finalize_passphrase_bytes` already builds a
`Zeroizing<String>` at line 370; wrap it as
`Passphrase::from_zeroizing(z)` on return. No other body changes
required.

### Replace `Zeroizing<String>` containers

`OpenCredential` and the flag-router `resolve_credential` live in
[`cli/src/credential.rs`](../../cli/src/credential.rs), not
`mount.rs`. Patch the owner module directly:

- `cli/src/credential.rs:11-14` `OpenCredential::Passphrase(Zeroizing<String>)`
  -> `OpenCredential::Passphrase(Passphrase)`.
- `cli/src/credential.rs:19-24` `OpenCredential::as_borrowed()`:
  the current body returns
  `Credential::Passphrase(pp.as_str())`; update it to
  `Credential::Passphrase(pp)` once `Credential::Passphrase` takes
  `&'a Passphrase` (see "Update `Credential::Passphrase`" below).
  This is a tracked secret-egress conversion and must change with the
  rest of commit 2.
- `cli/src/credential.rs:48-58` `resolve_credential` already binds
  `let pp = luks::read_passphrase(...)?;` -- after Improvement 2 the
  read pipeline returns `Passphrase`, so the `Ok(OpenCredential::Passphrase(pp))`
  line works without further change.
- `cli/src/credential.rs:27-34` `Debug` impl needs no body change but
  a unit test asserting the redaction lands here (see Verification).

- `cli/src/recover.rs:48-50` `RecoverPassphrase`:
  - `Borrowed(&'a Zeroizing<String>)` -> `Borrowed(&'a Passphrase)`.
  - `Owned(Zeroizing<String>)` -> `Owned(Passphrase)`.
- `cli/src/recover.rs:55-60` `RecoverPassphrase::as_str` becomes
  `expose_secret`. Update its eight call sites in the same file:
  lines 2079, 2097, 2130, 2154, 2178, 2188, 2454, 2466 -- each
  currently invokes `passphrase.as_str()` (or `p.as_str()` at 2079
  via `.map`) on a `RecoverPassphrase` and must rename to
  `expose_secret()`. Other `.as_str()` calls in `recover.rs` are
  against `String`/`&str`/mapper/label values and must stay
  unchanged.
- `cli/src/recover.rs:1762-1772` `open_credential_passphrase`'s body
  destructures the `OpenCredential::Passphrase` variant and currently
  returns `passphrase.as_str()` (line 1769). Update it to
  `passphrase.expose_secret()`. The function's `&'a str` return type
  stays unchanged -- callers (e.g. line 1828) keep working without
  edit.

### Test-fixture call sites that construct `OpenCredential` directly

These exist because `OpenCredential::Passphrase(...)` is currently
`Zeroizing<String>`; after the migration they construct a
`Passphrase`. Update each in commit 2:

- `cli/src/mount.rs:923-924`, `cli/src/mount.rs:3573` -- replace
  `OpenCredential::Passphrase(Zeroizing::new("testpass".to_owned()))`
  with `OpenCredential::Passphrase(Passphrase::from_zeroizing(Zeroizing::new("testpass".to_owned())))`,
  or use the test helper `zpass("testpass")` (see "Test helpers"
  below).
- `cli/src/recover.rs:8905`, `cli/src/recover.rs:9065`,
  `cli/src/recover.rs:9224` -- same pattern, three sites.

### Replace `passphrase: &str` parameters with `passphrase: &Passphrase`

12 production sites:

- `cli/src/luks.rs:396` (`luks_format`),
  `cli/src/luks.rs:502` (`verify_passphrase`),
  `cli/src/luks.rs:749` (`ensure_luks_open`),
  `cli/src/luks.rs:836` (`luks::enroll_key_file`).
- `cli/src/enroll_key_file.rs:153,271,895,908,946`.
- `cli/src/recover.rs:1846,1966,2369`.

The internal subprocess handoff in each of the four `luks.rs` sites
changes from `passphrase.as_bytes()` to
`passphrase.expose_secret().as_bytes()`. These four lines become the
documented secret-egress points.

### Update `Credential::Passphrase`

`cli/src/credential_verify.rs:13`:

```rust
pub enum Credential<'a> {
    Passphrase(&'a Passphrase),
    KeyFile(&'a Path),
}
```

Then at the verify call site (line 46):

```rust
Credential::Passphrase(passphrase) => {
    luks::verify_passphrase(runner, &target.device, passphrase)
}
```

`verify_passphrase` now takes `&Passphrase` (above), so this passes
through directly.

### Test helpers

- `cli/src/luks.rs:965` `fn zpass(s: &str) -> Zeroizing<String>` ->
  returns `Passphrase`.
- `ScriptedPassphraseReader::read_tty` (line 211) returns `Passphrase`.
- All test sites that pass `Credential::Passphrase("secret")` (e.g.
  `credential_verify.rs:181,308`) need to construct a `Passphrase`
  via `zpass("secret")` and pass `Credential::Passphrase(&p)`.
- Tests that pass `Zeroizing::new("...".to_owned())` directly
  (`mount.rs:923-924`, `mount.rs:3573`, `recover.rs:8905`,
  `recover.rs:9065`, `recover.rs:9224`) become
  `Passphrase::from_zeroizing(Zeroizing::new(...))` or use a new
  `Passphrase::from_str_for_test(&str)` helper gated on
  `#[cfg(test)]`.

---

## Improvement 3 -- local confirmation comparison

The only in-process passphrase comparison in the codebase is
`check_passphrase_match` (`cli/src/luks.rs:379-390`). Keep that
comparison local to the helper instead of adding `PartialEq` to
`Passphrase`:

```rust
fn check_passphrase_match(
    first: Passphrase,
    second: Passphrase,
) -> Result<Passphrase, LuksError> {
    if first.expose_secret() == second.expose_secret() {
        Ok(first)
    } else {
        Err(LuksError::Validation(
            "passphrases do not match -- aborting".to_owned(),
        ))
    }
}
```

This is an interactive typo check, not a security boundary. Normal
passphrase verification remains delegated to cryptsetup/LUKS.

---

## Critical files

| File | Change |
| --- | --- |
| `docs/decisions/023-secret-handling.md` | new -- ADR with frontmatter |
| `docs/index.md` | add 023 entry under `decisions/` |
| `cli/src/secret.rs` | new module -- `Passphrase` newtype (no `Clone`) |
| `cli/src/lib.rs` | `pub mod secret;` with `///` boundary doc comment |
| `cli/src/luks.rs` | pipeline + 4 boundary fns + test helpers |
| `cli/src/credential.rs` | `OpenCredential` variant payload, `as_borrowed()` returning `Credential::Passphrase(pp)`, and the new debug-redaction test |
| `cli/src/mount.rs` | test-fixture construction sites only (lines 923-924, 3573) |
| `cli/src/recover.rs` | `RecoverPassphrase` variants + `open_credential_passphrase` body (line 1769) + 8 `RecoverPassphrase::as_str` -> `expose_secret` call sites (lines 2079, 2097, 2130, 2154, 2178, 2188, 2454, 2466) + 3 `&str` params + 3 test fixtures (lines 8905, 9065, 9224) |
| `cli/src/enroll_key_file.rs` | 5 `&str` params |
| `cli/src/credential_verify.rs` | `Credential::Passphrase` payload |
| `cli/src/add.rs` | construction-side adjustments at line 693 |

---

## Verification

### Unit tests

- `just test-rust` -- the `cli/src/luks.rs` mod tests cover
  `read_passphrase_*`, `check_passphrase_match_*`, and the file/stdin
  byte-cap edge cases. Test signatures change with the newtype but
  behavioral assertions stay byte-identical.
- New unit test in `cli/src/secret.rs`:
  - `passphrase_debug_redacts` -- `format!("{:?}", p)` contains
    `<redacted>` and does not contain the plaintext for any of a
    short list of fixture inputs.
- New unit test in `cli/src/credential.rs` covering the existing
  `OpenCredential::Debug` impl
  ([`credential.rs:27-34`](../../cli/src/credential.rs#L27)). This
  was uncovered before this plan and is the other secret-owning
  `Debug` surface that could regress without notice. It belongs
  next to the type definition, not in `mount.rs`:
  - `open_credential_debug_redacts_passphrase` -- asserts
    `format!("{:?}", OpenCredential::Passphrase(p))` contains
    `<redacted>` and does not contain the plaintext, for a
    `Passphrase` constructed from a recognizable fixture string.
    Also exercise the `KeyFile` arm to confirm the path is shown
    (which is intended -- the path itself is not a secret).

### VM tests (no behavioral changes expected)

`just test-vm` against the registered flake check names that exercise
each secret-flow path. Names verified against
[`flake.nix`](../../flake.nix):

- `braid-unlock` -- baseline passphrase unlock.
- `braid-unlock-key-file` -- keyfile unlock.
- `braid-enroll` and `braid-enroll-generate` -- enroll existing
  keyfile and generate-new-keyfile paths.
- `braid-module-add-bootstrap` -- add flow that also drives the
  bootstrap LUKS format.
- `braid-recover` -- recovery passphrase flow through
  `RecoverPassphrase::Owned`/`Borrowed`.
- `replace-live-disk` and `replace-dead-disk` -- replace flow's
  passphrase verify and luks-format paths.

Together these touch every `Passphrase` boundary: read, verify,
format, open, enroll, recover, replace.

### Behavioral invariants to preserve

- Wrong passphrase still produces the same exit code and same stderr
  wording in `braid unlock` / `braid add` / `braid replace` /
  `braid recover` (existing tests anchor on these strings).
- New-format confirmation still requires byte-exact match.
- `OpenCredential` and `Passphrase` `Debug` output contains
  `<redacted>` and never the plaintext.
- Subprocess handoff still goes via stdin (existing
  `MockRunner::with_output_stdin` expectations still hold).

---

## Commit slicing

Two commits, in order:

1. `docs(adr): codify secret-handling discipline` -- ADR file with
   frontmatter, plus the `docs/index.md` entry. No code changes.
2. `refactor(cli): introduce Passphrase newtype`
   -- adds `cli/src/secret.rs` (no `Clone` derive), migrates the read
   pipeline (`luks.rs` + `OpenCredential` +
   `RecoverPassphrase` + `Credential`), and adds the
   `OpenCredential` debug-redaction test alongside the new
   `Passphrase` tests. All `passphrase: &str` parameters are
   migrated in this same commit because the read pipeline emits
   `Passphrase` and downstream signatures must accept it. The four
   `luks.rs` subprocess-handoff helpers (`luks_format`,
   `verify_passphrase`, `ensure_luks_open`, `luks::enroll_key_file`)
   also switch to `&Passphrase` here, with their internal
   `runner.run_with_stdin(req, ...)` calls converted to
   `passphrase.expose_secret().as_bytes()` -- splitting these out is
   not viable because the helpers and their callers must compile
   together.

---

## Out of scope

- `mlock(2)` / `mprotect(PROT_NONE)` for secret pages -- swap is
  typically off or encrypted on a NixOS NAS; CAP_IPC_LOCK overhead is
  not justified.
- Migrating to the `secrecy` crate -- `Passphrase` covers our needs
  with the dependency we already have (`zeroize`). `secrecy` would add
  API surface (`SecretBox`, `SecretSlice`, `ExposeSecret`/
  `ExposeSecretMut` traits) for marginal additional safety.
- Audit of state-file metadata -- `pool.json`, journal, alert latch,
  and acked-stats files contain UUIDs/paths/membership, not key
  material. No new work needed.

---

## Dependencies summary

No new dependency is added. The implementation continues to rely on
the existing `zeroize` dependency.
