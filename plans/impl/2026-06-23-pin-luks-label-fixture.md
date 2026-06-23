# Pin a populated `braid-<name>` LUKS label in the captured luksDump fixture

## Context

`discover.rs` reconstructs a cold pool by joining three parses over one
`cryptsetup luksDump` body: version (`parse_cryptsetup_luks_version`), UUID
(`parse_cryptsetup_luks_uuid_from_dump`), and label
(`parse_cryptsetup_luks_label`). The label is load-bearing: discover requires
`label = braid-<valid-name>` and skips any disk without it
(`cli/src/discover.rs#discover_from_dir_inner`).

The stable golden fixture lane exists to catch tool-version drift in real
captured output. Today that lane has an asymmetry on the label field: version
and UUID are pinned against the real captured values, but the stable captured
fixture carries `Label: (no label)`
in `cli/tests/fixtures/nixos-26.05/cryptsetup-luks-dump.txt`, because the
capture script formats with a bare `luksFormat` and no `--label`. So the only
field discover reads that braid *always populates in production* is pinned
against a value braid never writes. The populated-label parse is proven only by
synthetic inline strings (`extracts_braid_label`) and the slower VM lane.

This is low severity -- the authoritative renderer prints the label with a
plain `%s` after a fixed `Label:         \t` prefix
(`reference/cryptsetup/lib/luks2/luks2_json_metadata.c#LUKS2_hdr_dump`), with
no quoting or `Label[...]` variant, and the existing `(no label)` fixture
already pins the prefix/trim path. But the fix is cheap and makes the captured
dump represent an actual braid-formatted disk (a `(no label)` disk is never a
braid pool member), closing the asymmetry and giving all three discover parsers
a faithful golden body.

Intended outcome: the captured stable `cryptsetup-luks-dump.txt` carries a real
`braid-<name>` label, and the existing label golden test pins that the populated
value parses in the nixos-26.05 fixture lane while the stale unstable fixture
keeps an explicit `None` expectation until its capture lane can be repaired.

## Pivot from the original finding

The finding proposed *adding* a `parse_cryptsetup_luks_label` golden test
asserting `Some("braid-disk1")`. Two corrections:

1. **Modify, don't add.** A `golden_cryptsetup_luks_label` golden test already
   exists (`cli/tests/support/golden_common.rs#golden_cryptsetup_luks_label`)
   and currently asserts `out.label.is_none()` against this fixture. Once the
   fixture carries a label, that test fails; a second test asserting the
   opposite would be contradictory. The right move is to flip the existing
   assertion and its comment.

2. **Label per-disk, faithfully.** Use `--label braid-{disk}` in the existing
   format loop rather than a single hardcoded `braid-disk1`. This mirrors the
   loop's existing `braid-{disk}` mapper-open naming and real braid's
   `LuksLabel::for_disk` -> `braid-<name>`
   (`cli/src/types.rs#LuksLabel::for_disk`), and `braid-vdb` round-trips
   through `config::name_from_mapper` (`cli/src/config.rs#name_from_mapper`) +
   `DiskName::parse` (`cli/src/types.rs#DiskName::parse`). The dump source is
   `vdb`, so the asserted value is `Some("braid-vdb")`.

## Changes

### 1. Capture script -- `tests/capture-tool-fixtures.py`

In the initial LUKS setup loop, add `--label braid-{disk}` to the `luksFormat`
invocation, and add a comment noting the label is load-bearing for the captured
dump so a future reader does not "simplify" it away:

```python
# --- Set up LUKS on both disks ---
# Label each disk braid-<name>, mirroring what `braid add` writes
# (LuksLabel::for_disk -> braid-<name>). The captured luksDump of vdb then
# pins the populated-label parse discover joins with version + UUID; see
# golden_cryptsetup_luks_label.
for disk in ["vdb", "vdc"]:
    machine.succeed(
        f"echo -n '{PASSPHRASE}' | cryptsetup luksFormat --batch-mode --label braid-{disk} /dev/{disk} -"
    )
    machine.succeed(
        f"echo -n '{PASSPHRASE}' | cryptsetup open /dev/{disk} braid-{disk} -"
    )
```

`vdd` (formatted separately for the replace captures) is left unlabeled -- its
header is never dumped to a label fixture, so it is out of scope and labeling
it would only widen the diff.

### 2. Golden tests -- stable expectation + shared assertion

Define the per-suite expected label in the including test crates:

```rust
// cli/tests/golden_nixos_26_05.rs
const EXPECTED_LUKS_LABEL: Option<&str> = Some("braid-vdb");

// cli/tests/golden_nixos_unstable.rs
const EXPECTED_LUKS_LABEL: Option<&str> = None;
```

Then flip the shared assertion and rewrite the now-stale comment:

```rust
golden_test!(
    golden_cryptsetup_luks_label,
    "cryptsetup-luks-dump.txt",
    "cryptsetup luksDump",
    parse::cryptsetup_luks_label::parse_cryptsetup_luks_label,
    |out: parse::types::CryptsetupLuksLabelOutput| {
        // capture-tool-fixtures.py formats stable vdb with
        // `cryptsetup luksFormat --label braid-vdb`, mirroring the
        // braid-<name> label `braid add` writes (LuksLabel::for_disk).
        // The stable dump pins the populated-label parse discover joins with
        // version + UUID over one luksDump body. The unstable dump keeps its
        // explicit stale-fixture expectation until that lane can be recaptured.
        assert_eq!(out.label.as_deref(), EXPECTED_LUKS_LABEL);
    }
);
```

### 3. Re-capture stable fixtures (not hand-edited)

House rule is re-capture via the VM check, never hand-edit
(`docs/dev/parser-compatibility.md`, `cli/tests/fixtures/nixos-26.05/README.md`).
Regenerate from the edited script under the stable channel (NixOS VM via the
macOS `nix.linux-builder`):

- `just capture-fixtures`           # regenerates `cli/tests/fixtures/nixos-26.05/`

(`just capture-all-fixtures` also works; the luksDump fixture comes from the
`capture-tool-fixtures` check that this recipe builds.)

The `cryptsetup-luks-dump.txt` file is a regenerated artifact of this step, not
a file to edit by hand.

## What does NOT change

- `parse_cryptsetup_luks_label` and its unit tests -- the parser is unchanged;
  `returns_none_for_no_label` retains the `(no label)` -> `None` coverage.
- `golden_cryptsetup_luks_version`,
  `luks_uuid_from_dump_parses_nixos_26_05_fixture`, and both JSON-dump
  consumers -- they read other fields / the `.json` file.
- `cli/tests/fixtures/nixos-unstable/cryptsetup-luks-dump.txt` -- left stale on
  purpose because the unstable capture VM currently hangs before launching the
  guest QEMU.
- JSON-dump consumers and the JSON contract -- the LUKS2 label lives in the
  binary header, not the JSON metadata area, and the JSON parser reads no label,
  so adding `--label` does not propagate into `cryptsetup-luks-dump.json` or
  affect `golden_cryptsetup_luks_dump`. (The `.json` file is still regenerated
  with fresh random keyslot material on recapture, like every fixture -- that
  churn is unrelated to the label; see Verification.)
- Parser/type doc comments (`cryptsetup_luks_label.rs`, `parse/types.rs`) and
  ADR 024 -- they document parser behavior / CLI policy, which is unchanged. The
  parser doctest example already shows `braid-disk1`, consistent with a labeled
  dump.

## Critical files

- `tests/capture-tool-fixtures.py` -- add `--label braid-{disk}` to the initial
  LUKS setup loop.
- `cli/tests/golden_nixos_26_05.rs` -- set `EXPECTED_LUKS_LABEL` to
  `Some("braid-vdb")`.
- `cli/tests/golden_nixos_unstable.rs` -- set `EXPECTED_LUKS_LABEL` to `None`
  until the unstable fixture can be recaptured.
- `cli/tests/support/golden_common.rs` -- flip `golden_cryptsetup_luks_label`
  assertion + comment.
- `cli/tests/fixtures/nixos-26.05/cryptsetup-luks-dump.txt` (and the sibling
  `.json`) -- regenerated by re-capture. The `Label:` line becomes
  `braid-vdb`; UUID, salts, digest, and PBKDF Memory/Iterations also churn
  (they are random or benchmark-derived per format), which is expected.

## Verification

1. **TDD ordering (confirm it fails for the right reason).** Apply the
   `golden_common.rs` assertion flip *before* re-capturing, then run
   `just test-rust` -- `golden_cryptsetup_luks_label` must fail with
   `got: None` against the still-`(no label)` stable fixture. This proves the
   stable suite exercises the populated-label branch.
2. **Edit the script and re-capture** the stable channel (recipe above).
3. **Expect nondeterministic churn; verify the label, not a minimal diff.**
   Re-capture reruns `cryptsetup luksFormat` and `mkfs.btrfs`, so the
   regenerated `.txt` and `.json` fixtures change in UUID, keyslot/digest salts,
   digest, PBKDF Memory/Iterations, and btrfs FSIDs. This is expected and benign
   -- committed fixture refreshes already differ on exactly these fields -- so
   do *not* assert a one-line or empty diff. Instead require that the stable
   captured `cryptsetup-luks-dump.txt` has a populated `Label:` line reading
   `braid-vdb` (no longer `(no label)`); the parse to `Some("braid-vdb")` is
   then enforced by the green golden test in step 4.
4. **Green tests** (the authoritative gate):
   - `just test-rust` -- stable golden tests (`golden_cryptsetup_luks_label`
     now asserts `Some("braid-vdb")`; version + UUID still pass) plus the lib
     unit tests including `returns_none_for_no_label`.
   - `just test-parsers` -- the parser-compatibility lane. The unstable golden
     crate keeps asserting `None` for its stale fixture until the unstable
     capture lane can be regenerated.

## Implementation notes

- Scoped the implementation to the stable nixos-26.05 fixture lane after
  `just capture-fixtures-unstable` repeatedly hung at `machine: starting vm`
  before launching a per-test QEMU process.

## Follow Up

- Investigate `just capture-fixtures-unstable` startup hangs before refreshing
  `cli/tests/fixtures/nixos-unstable/cryptsetup-luks-dump.txt`; the unstable
  test driver reached `machine: starting vm` but did not launch the guest QEMU.
