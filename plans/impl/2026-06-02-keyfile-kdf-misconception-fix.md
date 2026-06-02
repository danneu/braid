# Fix the "raw key material / no PBKDF" keyfile misconception (repo-wide)

## Context

A review finding flagged that `docs/internals/luks-unlock.md` describes
braid's auto-unlock keyfile as raw key material "used directly... with no
derivation." That is factually wrong, and the wrong model has propagated:
the same false "no PBKDF / raw key material" claim now lives in **7 live,
authoritative files** -- two Rust doc comments, four VM-test preambles, and
the internals doc.

The truth, verified against the code and the cryptsetup man page:

- braid enrolls the keyfile with `cryptsetup luksAddKey --key-slot 1
  --new-keyfile-size 4096` (`cli/src/cmd.rs#CmdRequest` -> `LuksAddKeyFile`
  arm) and opens it with `cryptsetup open --type luks --key-file <path>
  --keyfile-size 4096` (`LuksOpenKeyFile` arm). Both are **LUKS keyslot**
  operations, so the keyfile's bytes are stretched by the keyslot KDF
  (Argon2id by default for LUKS2 -- braid always formats `--type luks2`,
  `cmd.rs` `LuksFormat` arm). It is **not** a raw dm-crypt volume key.
- The man page (`reference/cryptsetup/man/common_options.adoc`, `--key-file`
  entry) confirms the "passed directly in dm-crypt" / no-digest behavior is
  scoped to the **plain** device type, not LUKS; for LUKS the `--key-file`
  content "is always the passphrase for the existing keyslot."
- The real passphrase-vs-keyfile divergence is per-keyslot salt + transport
  + byte handling, not PBKDF-vs-no-PBKDF:
  - **Passphrase (slot 0):** braid trims a trailing `\n`/`\r` and rejects
    embedded line breaks (`cli/src/luks.rs#finalize_passphrase_bytes`), then
    pipes the bytes to cryptsetup stdin via `--key-file=-` (no
    `--keyfile-size`, since a passphrase is variable-length).
  - **Keyfile (slot 1):** braid reads exactly 4096 bytes from the file with
    `--keyfile-size 4096` and does *not* trim a trailing newline.
  - Both feed the keyslot KDF. Each keyslot has its own salt, so slot 0 and
    slot 1 derive different keys even from identical input -- that is the
    fundamental reason a passphrase and a keyfile are never interchangeable;
    the passphrase-newline trim is a secondary, braid-specific difference.
- A true raw volume key would need `--volume-key-file`, which braid forbids:
  it is in the `MANAGED_LUKS_FORMAT_LONG_FLAGS` denylist in
  `cli/src/types.rs`, so braid refuses to let it reach `luksFormat`. (The
  original finding claimed `rg volume-key-file cli/src` finds nothing -- it
  actually finds this denylist entry, which *strengthens* the point.)

The intended outcome: one coherent correction so no live file states or
implies the keyfile skips key derivation, and the internals doc accurately
explains the real model. No runtime behavior changes -- this is a
comments-and-docs accuracy pass.

## Ground-truth references (read before editing)

- `cli/src/cmd.rs` -- `LuksOpenKeyFile`, `TestKeyFile`, `LuksAddKeyFile`,
  `LuksFormat` arms of `CmdRequest::to_argv`; and the keyfile-size asymmetry
  block comment above the test `cryptsetup_luks_open_omits_keyfile_size`.
- `cli/src/luks.rs#finalize_passphrase_bytes` (passphrase newline trim) and
  `cli/src/luks.rs#LUKS_SLOT_KEYFILE`.
- `cli/src/types.rs` -- `MANAGED_LUKS_FORMAT_LONG_FLAGS` (the
  `--volume-key-file` denylist).
- `reference/cryptsetup/man/common_options.adoc` -- `--key-file` /
  `--keyfile-size` entries (plain-vs-LUKS distinction).

## The fix: 7 live files

For every edit, replace the false "no PBKDF / raw key material / used
directly / no derivation" framing with the true model (KDF-stretched
keyslot secret; divergence is transport + newline + slot, not derivation).
Keep each file's existing structure and register; only the wrong clause
changes.

### 1. `cli/src/luks.rs` -- two doc comments

- **Line 20**, on `LUKS_SLOT_KEYFILE`. Current:
  `/// LUKS key slot 1: binary random keyfile (no PBKDF, raw key material).`
  Replace with an intent/invariant comment (per the repo Doc Comments rule),
  e.g.:
  `/// LUKS keyslot 1 holds the auto-unlock keyfile -- a high-entropy`
  `/// 4096-byte secret enrolled and opened as a keyslot passphrase`
  `/// (KDF-stretched like any LUKS secret, not a raw volume key). Slot 0`
  `/// is the interactive passphrase.`
- **Line 985**, on `ensure_luks_open_with_key_file`. Current:
  `/// Open a LUKS device with a binary keyfile (no passphrase, no PBKDF).`
  Replace, e.g.:
  `/// Keyfile counterpart to the stdin-passphrase open: feeds the keyfile`
  `/// via --key-file/--keyfile-size instead of piping a passphrase. Still a`
  `/// KDF-stretched keyslot secret, not a raw volume key.`

### 2. Test preambles -- four files (`.nix` + `.py` pairs)

Fix only the wrong parenthetical; preserve the true distinctions (different
flags, file-fed vs stdin, `run()` vs `run_with_stdin()`, explicit slot) and
each file's existing preamble shape (`.nix` uses `What:/Why:`; `.py` uses
`Intent:/Why it exists:/Scenario:`). Keep the `.nix`/`.py` pair in sync on
substance.

- `tests/cli/braid-unlock-key-file.nix:6-8` and
  `tests/cli/braid-unlock-key-file.py:6-9`: drop `no PBKDF` from
  "(no PBKDF, different cryptsetup flags, run() vs run_with_stdin)". Replace
  with the real distinction, e.g. "file-fed `--key-file`/`--keyfile-size`
  vs a piped stdin passphrase, and `run()` vs `run_with_stdin()` -- both are
  KDF-stretched LUKS keyslots; the divergence is transport and flags, not
  derivation."
- `tests/cli/braid-enroll.nix:7-9` and `tests/cli/braid-enroll.py:7-10`:
  replace "(raw bytes, explicit slot, no PBKDF)" with, e.g. "a fixed
  4096-byte file fed with `--keyfile-size` into an explicit slot 1, vs a
  variable-length stdin passphrase in slot 0 -- both KDF-stretched keyslots,
  not raw key material." Preserve the "auto-unlock breaks at 3 AM" line.

### 3. `docs/internals/luks-unlock.md` -- the "Passphrase file vs binary keyfile" section (lines ~30-52)

Keep the heading text verbatim (`## Passphrase file vs binary keyfile`) --
there's no reason to rename it, and an unchanged slug is trivially
linkcheck-safe. (No doc currently links to this anchor; `mdbook-linkcheck2`
would catch any future breakage regardless.) Rewrite the body to:

- Open with: braid enrolls/opens **both** the shared passphrase and the
  keyfile as LUKS *keyslot* secrets, so both are stretched by the keyslot
  KDF (Argon2id by default for LUKS2). Neither is a raw dm-crypt volume key.
  Say "by default", not an unconditional "Argon2id": `--pbkdf` is not in the
  `MANAGED_LUKS_FORMAT_LONG_FLAGS` denylist, so an operator can override the
  passphrase slot's KDF via `--luks-format-arg=--pbkdf=...`. The doc prose
  needs only "by default" -- do not document the override mechanism inline.
- Passphrase bullet (slot 0): braid trims the trailing newline and rejects
  embedded line breaks (cite `cli/src/luks.rs#finalize_passphrase_bytes` as
  a code span, not a link), then pipes via `--key-file=-` with no
  `--keyfile-size`. Drop the inaccurate "read until first newline" /
  "PBKDF2 (LUKS1)" framing (braid does the trimming itself and always uses
  LUKS2).
- Keyfile bullet (slot 1): exactly 4096 bytes via `--keyfile-size 4096`
  (braid enforces the exact size with
  `cli/src/luks.rs#validate_user_keyfile_path`), no newline trim; high
  entropy, but still a KDF-protected keyslot, not a raw key.
- Divergence paragraph: lead with the fundamental reason -- each LUKS keyslot
  has its own salt, so slot 0 and slot 1 derive different keys *even from
  identical KDF input*; that is why a passphrase and a keyfile are never
  interchangeable. Then, as a secondary cryptsetup-level point, the bytes
  that reach the KDF can also differ: a passphrase file `hunter2\n` feeds
  `hunter2` (newline trimmed) while a keyfile of the same bytes feeds
  `hunter2\n` verbatim. Frame this byte example as illustrative -- braid's
  keyfile is always exactly 4096 random bytes (rejected otherwise by
  `validate_user_keyfile_path`), so the literal "same bytes" case cannot
  actually arise in braid. The claim to kill is "one skips a KDF": both run
  the KDF.
- Add: raw-volume-key use would require `--volume-key-file`, which braid
  forbids via the `MANAGED_LUKS_FORMAT_LONG_FLAGS` denylist in
  `cli/src/types.rs`. Cross-reference the keyfile-size argv asymmetry pinned
  in the block comment above the test
  `cryptsetup_luks_open_omits_keyfile_size` in `cli/src/cmd.rs`.
- Fix the slot-count nit: "up to 8 slots per device" -> LUKS2 provides up to
  32 keyslots; braid uses slot 0 (passphrase) and slot 1 (keyfile).
- In the `See:` line, note the man page's "passed directly in dm-crypt"
  behavior is scoped to the *plain* device type, not LUKS.

Use ASCII `--` (not em-dash) in all new/edited prose, per house style.

## Deliberately out of scope

- `plans/impl/2026-01-01-predated/auto-unlock2.md` and
  `plans/impl/.../plan-auto-unlock.md:186` repeat the wrong framing, but
  they are dated historical implementation records -- rewriting them would
  falsify history. Leave untouched.
- `docs/design/principles.md:36` and
  `plans/impl/2026-06-02-principle-4-keyfile-usage.md:57` say the keyfile can
  be "passed directly to `braid unlock --key-file`" -- this means *supplied
  to the command*, not *used as a raw key*. Accurate; do not change.

## Verification

Comments-and-docs only; no behavior changes, so no VM suite run is needed
(the `.nix`/`.py` edits are inert test preambles).

1. **Prove the misconception is gone** -- re-run the discovery sweep and
   confirm zero hits outside `plans/` and `reference/`:
   ```
   rg -n -i "no pbkdf|no derivation|raw key material|raw volume key|used directly as key|directly as key material" --glob '!plans/**' --glob '!reference/**'
   ```
   This sweep is necessary but not sufficient. The two `braid-enroll`
   preambles read "(raw bytes, explicit slot, no PBKDF)", and the regex
   deliberately omits bare `raw bytes`: that token is not itself false, and
   `tests/storage/btrfs-heal.py` uses "raw bytes" legitimately (broadening
   the regex would create a permanent false positive there). So after
   editing, eyeball both `braid-enroll` preambles (`.nix` + `.py`) to confirm
   the *whole* wrong parenthetical -- not just "no PBKDF" -- was replaced.
2. **Docs build + linkcheck** -- `mdbook build docs` must pass (validates the
   new `path#anchor`/code-span references and that the heading anchor is
   intact). Broken cross-links fail this build.
3. **Rust sanity** -- `just test-rust` (doc-comment edits are inert, but this
   is the canonical check that nothing else regressed).

## Implementation notes

- The plan's suggested negation wordings ("not a raw volume key", "not raw
  key material") contain the exact tokens the Verification step-1 sweep greps
  for, so shipping them verbatim would leave the sweep with permanent
  false-positive hits -- defeating it as a regression guard, since a future
  real regression would hide among the negations. Resolved by standardizing
  the negations on the plan's own Context phrasing without changing meaning:
  "raw dm-crypt volume key" in the two `cli/src/luks.rs` doc comments and in
  `docs/internals/luks-unlock.md` (the regex matches the contiguous "raw
  volume key", which the inserted "dm-crypt" breaks), and "unstretched key
  material" in the two `braid-enroll` preambles. The sweep now returns the
  zero hits the plan intended.
- The em-dash in the unrelated `## USB device naming stability` "See:" line
  (above the edited section) was left as-is; the house ASCII-`--` rule was
  applied only to new/edited prose, per the plan's explicit scope.
