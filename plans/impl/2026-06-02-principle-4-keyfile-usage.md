# Fix Principle 4 keyfile-usage overstatement

## Context

`docs/design/principles.md` is the authoritative invariants doc ("if code
or config contradicts a principle, the code is wrong"). Principle 4 (Single
passphrase), line 36, currently ends:

> The passphrase (slot 0) remains the interactive-unlock mechanism; keyfiles
> are for unattended auto-unlock only.

The "auto-unlock only" clause is false and self-contradictory:

- **Code:** `braid unlock --key-file <path>` is a fully wired, first-class
  operator command. The flag (`cli/src/main.rs:374-376`) flows through
  dispatch (`cli/src/main.rs:726`) and planning (`cli/src/unlock.rs:222`)
  into a real keyfile-backed LUKS open -- `compile_open_steps` emits
  `CryptsetupLuksOpenKeyFile` whenever a keyfile is supplied
  (`cli/src/mount.rs:367-376`). It is not a stub.
- **Internal contradiction:** the *same* Principle 4, one paragraph up at
  line 34, already states the keyfile-credential rule "applies to keyfile
  credentials used by `mount`, `unlock`, and `recover`" -- i.e. keyfiles are
  used by the manual `unlock` path.
- **Sibling docs already bless the manual path:** `docs/commands/enroll.md:5`
  ("`braid unlock --key-file` can open the pool without typing a
  passphrase"), `docs/commands/unlock.md:30-34,57`, and
  `docs/internals/luks-unlock.md:74` all document `braid unlock --key-file`
  as a supported operator path that reads ordinary admin-controlled paths.

A reader of line 36 concludes the slot-1 keyfile is reachable only via the
auto-unlock service. Intended outcome: the principle states the truth --
slot 1 serves both `braid.autoUnlock` (unattended) and the manual
`braid unlock --key-file` command -- bringing the principle into line with
line 34, the code, and the user-facing docs.

## Scope

Documentation only. One line in one file. No code change. No sibling
overstatements exist elsewhere (verified: `rg "auto-unlock only"` across
`docs/` + `README.md` matches only this line), so this is not a sweep.

## The fix

File: `docs/design/principles.md`, line 36.

Replace:

> Binary keyfile support is available via `braid enroll` (slot 1) and
> `braid.autoUnlock` (NixOS module). The passphrase (slot 0) remains the
> interactive-unlock mechanism; keyfiles are for unattended auto-unlock only.

With:

> Binary keyfile support is available via `braid enroll` (slot 1) and
> `braid.autoUnlock` (NixOS module). The passphrase (slot 0) is the default
> interactive-unlock mechanism; the slot-1 keyfile drives `braid.autoUnlock`
> for unattended boots and can also be passed directly to
> `braid unlock --key-file`.

Why this wording:

- Names the concrete command `braid unlock --key-file`, matching how
  `enroll.md` and `unlock.md` already refer to it.
- Reserves "interactive" for the *prompted* passphrase path and avoids
  calling the keyfile path "interactive" -- `--key-file` reads bytes from a
  file and never prompts, a distinction `docs/internals/luks-unlock.md:30-49`
  is careful about. (This is the one refinement over the originally proposed
  fix text, which described the keyfile path as "supplied interactively.")
- Keeps "default" on the passphrase clause: the passphrase is the default
  (no flag needed); the keyfile requires `--key-file`.
- Surgical -- preserves the first sentence and the principle's existing
  voice/rhythm. A fuller two-sentence rewrite was considered and rejected to
  minimize review surface on an authoritative invariants doc.

ASCII/dash note: the edit introduces no em-dash and no new markdown link, so
it neither conflicts with the repo's CLI-output dash rule nor adds anything
for mdbook-linkcheck2 to validate.

## Verification

1. **Proofread** the rendered sentence in `docs/design/principles.md:36`;
   confirm it reads cleanly and no longer contains "auto-unlock only".
2. **Confirm the overstatement is gone and unique:** re-run
   `rg -n "auto-unlock only" docs/ README.md` -- expect zero matches.
3. **Build the book** so the change is exercised by the same path CI uses:
   `mdbook build docs` (validates the tree via `mdbook-linkcheck2`; expect a
   clean build).
4. No Rust/VM tests required -- this is a docs-only change; `just test-*` is
   not in scope.
