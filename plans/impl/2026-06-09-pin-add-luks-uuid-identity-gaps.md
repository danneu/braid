# Plan: pin the two missing LUKS-UUID identity gaps for fresh `braid add`

## Context

[ADR 024 -- LUKS UUID Is Disk Identity](../../docs/design/decisions/024-luks-uuid-identity.md)
makes two add-side guarantees law:

1. **Journaled key == on-disk UUID.** A fresh add pre-generates the LUKS UUID
   (`LuksUuid::new_v4()` in `cli/src/add.rs#build_add_work_plan`), journals it as
   the map key before mutation, passes it through `--uuid` to cryptsetup
   (`cli/src/cmd.rs#luks_format_argv`), and writes it as the `pool.json` membership map
   key (`cli/src/membership.rs` `LuksUuidMap` / `DiskMember`). So the disk's
   live header UUID must equal its `pool.json` key.
2. **Extras may not override identity.** `--luks-format-arg=--uuid=...`
   (or `--label`) is rejected by `LuksFormatExtraOpts::parse`
   (`cli/src/types.rs` `is_managed_format_flag`) before any probe, journal
   write, or format (in `cli/src/add.rs#plan_add`), surfacing as
   `AddError::ManagedFormatFlag` (`#[error("{0}")]`, transparent) and a
   nonzero CLI exit.

**What is already covered.** Guarantee (1) is pinned end-to-end on the
*final stable state* by `tests/cli/braid-status-rust.py`: it adds disk1-3 via
real `braid add` (with `--luks-format-arg` extras), reads `cryptsetup luksUUID`
for each, and asserts `key_by_name[name] == real_uuid`
(its "Healthy JSON" subtest, "pool.json key for {name} != real LUKS UUID").
A whole-pool equality loop in `braid-add-disk.py` would only re-prove this.

**What is genuinely missing -- the two gaps this plan closes:**

- **(1a) Pre-balance write window.** `braid-status-rust.py` checks equality only
  after the pool settles. Nothing pins that the key braid persists *before* the
  post-add balance (the ADR-022 persist-before-balance path) already equals the
  live header. `braid-add-persists-before-balance.py` reaches into that exact
  window but today asserts only that the key is canonical-UUID-shaped and lacks a
  value-side `luks_uuid` -- not that it equals the disk's real UUID.
- **(2) Managed-flag refusal CLI wiring.** Guarantee (2) is covered only by
  pure-`parse` unit tests (`cli/src/types.rs`) and a `plan_add` mock-runner test
  (`cli/src/add.rs#add_rejects_managed_luks_format_args`). The black-box CLI wiring
  -- clap collection -> parse
  -> reject -> nonzero exit -> stderr wording, with **no state change** -- is
  untested. A regression that let the extra reach cryptsetup, or that wrote a
  journal/membership before rejecting, would leave the whole suite green.

**Outcome:** two test-only additions that close (1a) and (2). No production code
changes, no new test file, no `.nix` change.

## Approach

Two edits, both reusing existing tests and disks:

- Add the pre-balance `cryptsetup luksUUID == pool.json key` assertion to
  `braid-add-persists-before-balance.py`, where the key is already extracted.
- Add a black-box managed-flag refusal subtest to `braid-add-disk.py`'s existing
  Phase 5 ("Identity check refusals"), reusing the still-raw `disk4` and
  asserting full no-state-change.

## Changes

### 1. `tests/cli/braid-add-disk.py` -- managed-flag refusal subtest

Insert as the **first** subtest of "Phase 5: Identity check refusals", i.e.
immediately before the existing "Non-braid LUKS disk is refused" subtest. At
that point the pool is a settled disk1-3 RAID1, so `pool.json` exists and is
stable, and `disk4` is still raw -- that subtest's `cryptsetup luksFormat` is the
first thing to touch disk4. Because rejection is pre-mutation, `disk4` stays raw
afterward and the existing disk4 cleanup/add flow runs unchanged.

```python
with subtest("Managed --luks-format-arg=--uuid is refused with no state change"):
    # Intent: a user extra targeting braid-managed identity (--uuid) is rejected
    # at the CLI boundary, fail-closed -- nonzero exit, the managed-flag wording
    # on stderr with empty stdout, and ZERO side effects: disk4 stays raw, no
    # pending-op.json, pool.json byte identical.
    # Why it exists: LuksFormatExtraOpts::parse rejection is unit-tested, but the
    # CLI wiring (clap collection -> parse -> reject -> stderr -> exit; no
    # mutation) was untested end-to-end. A regression that let the extra reach
    # cryptsetup, wrote a journal/membership before rejecting, or routed the
    # error to stdout, would slip past the units.
    # Scenario: operator fat-fingers `--luks-format-arg=--uuid=...`; braid must
    # refuse before touching the raw disk or any state file.
    dev = "/dev/disk/by-id/virtio-disk4"
    pq = shlex.quote(passphrase)
    machine.succeed("cp /var/lib/braid/pool.json /tmp/pool-before-refusal.json")
    # No pipe: feed the passphrase via --passphrase-file so braid is the sole
    # process and `rc` is unambiguously braid's exit. A piped `printf | braid`
    # could SIGPIPE under `set -euo pipefail` and inflate `rc` to nonzero even if
    # braid wrongly exited 0, masking the nonzero-exit contract.
    # Separate stdout/stderr redirects: a regression routing the error to stdout
    # (stderr routing is part of the uncovered wiring) must fail here.
    machine.succeed(f"printf '%s\\n' {pq} > /tmp/refuse-pass")
    rc, _ = machine.execute(
        f"braid add --luks-format-arg=--uuid=11111111-1111-1111-1111-111111111111 "
        f"disk4={dev} --passphrase-file /tmp/refuse-pass --yes "
        f">/tmp/refuse-out 2>/tmp/refuse-err"
    )
    out = machine.succeed("cat /tmp/refuse-out")
    err = machine.succeed("cat /tmp/refuse-err")
    assert rc != 0, f"add must refuse a managed --uuid extra:\nstdout={out!r}\nstderr={err!r}"
    assert out == "", f"refusal must keep stdout empty; got: {out!r}"
    assert "targets a braid-managed identity" in err, (
        f"managed-flag rejection wording must be on stderr:\n{err}"
    )
    # Fail-closed: rejection happens in plan_add, before any probe or format.
    assert machine.execute(f"cryptsetup isLuks {dev}")[0] != 0, (
        "disk4 must remain unformatted after a refused add"
    )
    machine.fail("test -e /var/lib/braid/pending-op.json")
    machine.succeed("cmp /tmp/pool-before-refusal.json /var/lib/braid/pool.json")
```

The byte-identical `cmp` (matching the idiom in
`braid-add-uuid-swap-rejected.py` and `braid-add-persists-before-balance.py`)
subsumes any "disk4 membership leak" check. No `import json` is needed.

Also update `braid-add-disk.py`'s `# What:` header comment to list the new
managed `--luks-format-arg` refusal alongside the existing identity-check
refusals (doc-sync per AGENTS.md). `braid-add-disk.nix` needs **no** change.

Notes:
- `--luks-format-arg` is declared `require_equals = true` (`cli/src/main.rs`),
  so the `--luks-format-arg=--uuid=<v>` equals-form is mandatory. The `--uuid`
  value is irrelevant -- rejection fires on the token before value validation.
- The error string is transparent through `AddError::ManagedFormatFlag`
  (`#[error("{0}")]`); full wording is `--luks-format-arg '<token>' targets a
  braid-managed identity or storage-model-breaking cryptsetup option`, so the
  `targets a braid-managed identity` substring is a stable assert.
- The NixOS driver auto-prepends `set -euo pipefail` to every command (see
  `docs/dev/testing.md`). That is why the passphrase comes from a file, not a
  `printf | braid` pipe: with no upstream process there is no SIGPIPE to inflate
  `rc`, so `rc` is exactly braid's process exit and a regression that wrongly
  exits 0 (even while closing stdin early) cannot hide behind a nonzero pipeline.
  The assert stays `rc != 0` (contract-level), not an exact code.

### 2. `tests/cli/braid-add-persists-before-balance.py` -- pre-balance equality

In the existing "pool.json already contains the new disk during balance" block,
`disk2_uuid` is already extracted from the membership key. Add one assertion
right after it, pinning that the pre-balance-persisted key equals the live
header (the (1a) gap):

```python
assert machine.succeed("cryptsetup luksUUID /dev/disk/by-id/virtio-disk2").strip() == disk2_uuid, (
    f"disk2 pool.json key must equal its live LUKS UUID:\n{pool_json}"
)
```

Extend the `# Why it exists:` preamble with a sentence noting it also pins
on-disk == journaled UUID at the pre-balance write window (distinct from
`braid-status-rust.py`, which only checks the settled final state).

## Why this shape (not alternatives)

- **No whole-pool equality loop in `braid-add-disk.py`:** final-state guarantee
  (1) is already registered in `braid-status-rust.py` (its "Healthy JSON"
  subtest); a second loop adds device breadth but no new behavior. The only
  equality moment left unpinned is the pre-balance window, covered by edit 2.
- **VM, not Rust, for the refusal:** guarantee (2)'s parse and `plan_add` layers
  are already unit-covered; the VM test deliberately adds the *un*covered black-box
  wiring -- braid's own nonzero exit (passphrase fed from a file, not a pipe, so
  `rc` is braid's exit and not a `printf` SIGPIPE artifact), the error on
  **stderr with empty stdout** (separate `>/tmp/refuse-out 2>/tmp/refuse-err`
  redirects, not `2>&1`, so a stdout-routing regression fails), and -- via `cmp`
  + no `pending-op.json` -- the fail-closed no-state-change promise.
- **Reuse existing disk4, no `disk6`:** the refusal only needs a raw target, and
  `disk4` is raw at the start of Phase 5. Provisioning a sixth `emptyDiskImages`
  entry would add Nix churn and VM device surface for no coverage gain.
- **Inline, not shared helper:** Python VM scripts cannot share code (NixOS loads
  each `.py` via `builtins.readFile` with injected globals; `member`/`member_names`
  are copy-pasted across ~30 files). Do not attempt a shared-helper extraction.

## Verification

- `just test-vm braid-add-disk` (or the project's single-test invocation per
  `docs/dev/testing.md`) -- the new refusal subtest passes; the existing
  "Non-braid LUKS disk is refused" subtest still finds `disk4` raw and unchanged.
- `just test-vm braid-add-persists-before-balance` -- still green with the added
  equality line.
- Sanity that each new assertion is *load-bearing*, not vacuous: temporarily
  break it (point the equality at a wrong UUID; stub the refusal to exit 0; or
  route the refusal error to stdout to confirm the empty-stdout assert bites)
  and confirm the subtest fails, then revert.
- No `flake.nix` or `.nix` edit is required: both tests are already registered
  and no new disk is provisioned.

## Out of scope

- The copy-paste duplication of `member`/`member_names`/`missing_devid` across
  `tests/cli/*.py` is structural (framework can't import shared code) -- not
  addressed here.
- No production code changes: guarantees (1) and (2) are already implemented and
  correct; this plan only adds the two missing end-to-end regression pins.
