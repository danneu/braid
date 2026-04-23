# Verification: `--keyfile-size` asymmetry finding

## Context

A code reviewer flagged an asymmetry in `cli/src/cmd.rs`: the "keyfile pair"
(`CryptsetupLuksOpenKeyFile`, `CryptsetupTestKeyFile`) passes
`--keyfile-size 4096`, while the "passphrase pair" (`CryptsetupLuksOpen`,
`CryptsetupTestPassphrase`) omits it. The reviewer's revised claim was that
the two pairs are internally consistent but the cross-pair asymmetry is a
bug fixable by a "single-constant fix" (add `--keyfile-size 4096` to the
passphrase pair).

## Verification

The asymmetry exists as described, **but it is intentional and semantically
required.** Adopting the proposed fix would break passphrase unlock.

### What braid feeds to each pair

| Variant                        | `--key-file` source           | stdin/file bytes                           |
| ------------------------------ | ----------------------------- | ------------------------------------------ |
| `CryptsetupLuksOpen`           | `--key-file=-` (stdin)        | `passphrase.as_bytes()` -- variable-length |
| `CryptsetupTestPassphrase`     | `--key-file=-` (stdin)        | `passphrase.as_bytes()` -- variable-length |
| `CryptsetupLuksFormat`         | `--key-file=-` (stdin)        | `passphrase.as_bytes()` -- variable-length |
| `CryptsetupLuksOpenKeyFile`    | `--key-file <path>` (file)    | 4096-byte binary keyfile                   |
| `CryptsetupTestKeyFile`        | `--key-file <path>` (file)    | 4096-byte binary keyfile                   |

`cli/src/luks.rs:109`, `:180`, `:320` all pass `passphrase.as_bytes()` with
no padding. Unit tests use short strings like `b"testpass"` (8 bytes). The
4096 on the keyfile side comes from the enrollment contract in
`CryptsetupLuksAddKeyFile` (`cmd.rs:743-744`, `--new-keyfile-size 4096`).

### What `--keyfile-size` would do to the passphrase pair

`reference/cryptsetup/lib/utils.c:314-317`:

```c
if (!unlimited_read && i != key_size) {
    log_err(cd, _("Cannot read requested amount of data."));
    goto out;
}
```

With `--keyfile-size 4096`, the reader demands exactly 4096 bytes from stdin.
The non-interactive branch in
`reference/cryptsetup/src/utils_password.c:296-302` enforces that size when
`--key-file=-` is combined with piped stdin (braid's exact shape). A short
passphrase produces fewer bytes -> the error path fires and every unlock
fails.

The keyfile pair's `--keyfile-size 4096` is defensive: pins the read length
to the enrollment size so a tampered or truncated key file fails fast
instead of being silently interpreted with different bytes. That rationale
does not transfer to a variable-length user passphrase.

## Plan

The finding stays closed as "won't fix" on the code side. To prevent a
future contributor from re-opening the same bug by "normalizing" the
asymmetry, pin the exact argv for both pairs in unit tests. A naive fix
(adding `--keyfile-size` to passphrase, or dropping it from keyfile) then
fails tests immediately with a comment explaining why.

### Changes

File: `cli/src/cmd.rs` (tests module starts at line 1012).

Add six `#[test]` functions in the same style as the existing
`btrfs_replace_status_includes_minus_one` test (`cmd.rs:1234-1256`), each
asserting the full `cmd.args` vector against a literal. Every test carries
the Intent/Why/Scenario comment required by the project test conventions
and explicitly names the asymmetry so the next reader understands why the
two groups differ.

Test list (one per variant):

1. `cryptsetup_luks_open_omits_keyfile_size` -- pins
   `CryptsetupLuksOpen { device: "/dev/disk/by-id/disk1", mapper: "braid-disk1" }`
   to exactly:
   `["open", "--type", "luks", "--key-file=-", "--perf-no_read_workqueue", "--perf-no_write_workqueue", "/dev/disk/by-id/disk1", "braid-disk1"]`.
2. `cryptsetup_test_passphrase_omits_keyfile_size` -- pins
   `CryptsetupTestPassphrase { device: "/dev/disk/by-id/disk1" }` to
   `["open", "--test-passphrase", "--key-file=-", "/dev/disk/by-id/disk1"]`.
3. `cryptsetup_luks_format_omits_keyfile_size` -- pins
   `CryptsetupLuksFormat { device: "/dev/disk/by-id/disk1", extra_opts: vec![] }`
   to
   `["luksFormat", "--type", "luks2", "--batch-mode", "--key-file=-", "/dev/disk/by-id/disk1"]`.
4. `cryptsetup_luks_open_key_file_sets_keyfile_size_4096` -- pins
   `CryptsetupLuksOpenKeyFile { device: "/dev/disk/by-id/disk1", mapper: "braid-disk1", key_file_path: "/var/lib/braid/keyfiles/braid-disk1.key" }`
   to the full 10-arg vector including `--keyfile-size 4096`.
5. `cryptsetup_test_key_file_sets_keyfile_size_4096` -- pins
   `CryptsetupTestKeyFile { device: "/dev/disk/by-id/disk1", key_file_path: "/var/lib/braid/keyfiles/braid-disk1.key" }`
   to the full 6-arg vector including `--keyfile-size 4096`.
6. `cryptsetup_luks_add_key_file_sets_new_keyfile_size_4096` -- pins
   `CryptsetupLuksAddKeyFile { device: "/dev/disk/by-id/disk1", key_file_path: "/var/lib/braid/keyfiles/braid-disk1.key" }`
   to the full 6-arg vector including `--new-keyfile-size 4096`. This is
   the source of truth for the 4096 constant; other tests reference it.

### Test comment template (adapt per variant)

```
// Intent: lock in that stdin-fed cryptsetup invocations do NOT pass
// --keyfile-size.
// Why: with --key-file=- on piped stdin, cryptsetup's non-interactive
// branch (reference/cryptsetup/src/utils_password.c:296-302 ->
// lib/utils.c:314-317) demands exactly N bytes when --keyfile-size N is
// set. A user passphrase is variable-length and shorter than any sane
// N, so adding --keyfile-size would fail every unlock with "Cannot read
// requested amount of data". The keyfile variants (OpenKeyFile /
// TestKeyFile / AddKeyFile) DO pass 4096 because they read a fixed-size
// binary blob from a file -- see those tests for the symmetric pin.
// Scenario: a future "cleanup" PR normalizes the asymmetry by copying
// --keyfile-size 4096 into this variant's argv. This test fails
// immediately.
```

On the keyfile side, the comment inverts: the flag is load-bearing because
it pins the read to the enrollment size (`CryptsetupLuksAddKeyFile` writes
4096 bytes; silently reading a different count after file tampering would
produce a different derived key).

## Critical files

- `cli/src/cmd.rs:421-435` -- `CryptsetupLuksOpen`.
- `cli/src/cmd.rs:656-672` -- `CryptsetupLuksFormat`.
- `cli/src/cmd.rs:674-682` -- `CryptsetupTestPassphrase`.
- `cli/src/cmd.rs:699-718` -- `CryptsetupLuksOpenKeyFile`.
- `cli/src/cmd.rs:719-733` -- `CryptsetupTestKeyFile`.
- `cli/src/cmd.rs:734-748` -- `CryptsetupLuksAddKeyFile` (source of the
  4096 constant).
- `cli/src/cmd.rs:1012+` -- tests module; new tests go here.
- `cli/src/cmd.rs:1234-1256` -- existing `btrfs_replace_status_includes_minus_one`
  test, the style reference.
- `cli/src/luks.rs:97-120,170-183,301-331` -- passphrase callers that
  confirm `passphrase.as_bytes()` has no padding.
- `reference/cryptsetup/src/utils_password.c:260-317` -- stdin read branch.
- `reference/cryptsetup/lib/utils.c:182-329` -- `crypt_keyfile_device_read`
  and the "Cannot read requested amount of data" failure.

## Verification plan

1. Write all six tests and confirm they pass against the current code:
   `cargo test -p braid --lib cmd::tests -- cryptsetup`.
2. Regression check: temporarily add `--keyfile-size 4096` to
   `CryptsetupLuksOpen`'s argv in `cmd.rs`, rerun `cargo test` -> the
   corresponding test must fail with a clear message pointing at the
   pinned vector. Revert.
3. Regression check (inverse): temporarily remove `--keyfile-size 4096`
   from `CryptsetupLuksOpenKeyFile`, rerun `cargo test` -> the keyfile
   test must fail. Revert.
4. `just test-rust` as a final sanity pass.

No fixture refresh needed (no parser-critical tool version change).
