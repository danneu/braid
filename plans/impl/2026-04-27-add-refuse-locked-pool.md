# braid add silently bootstraps a new pool when the existing pool is locked

## Context

`braid add <name>=<by-id>` against a fresh disk, when `pool.json` already
lists locked members and the pool is not mounted, currently plans (and on a
real run will execute) a one-disk `mkfs.btrfs` bootstrap at the configured
mount point. `pool.json` then gets overwritten with the single new member
and the two existing locked members become orphaned. The user does not see
any warning. They are recoverable via `braid discover`, but they will not
notice anything is wrong until they realise their data is gone.

Why it happens (verified):

- `compile_add_steps_multi` at `cli/src/add.rs:1006` enters the bootstrap
  branch on `if !input.pool.mounted { ... }` with no check that
  `pool_membership.disks` is empty.
- `validate_braid_preconditions` (`cli/src/add.rs:83-106`) does fire the
  `!pool.mounted` refusal -- but only on the `PresentLuks` braid-labeled
  arm. A fresh `PresentNotLuks` disk skips it entirely.
- `plan_add` already loads `pool_membership` (`cli/src/add.rs:701`) and
  probes `pool` (`cli/src/add.rs:735`), but the `if pool.mounted` gate at
  `cli/src/add.rs:746` only adds *more* checks when mounted -- there is no
  symmetric "exists-but-not-mounted" refusal.

The fix is a single new preflight in the existing chain that refuses any
add against a locked-but-populated pool, regardless of the new disk's
state. The invariant is: "membership-mutating commands require an unlocked
pool whenever pool.json already lists members."

## Approach

Add a new preflight helper and call it from `plan_add` between the pool
probe and the mounted-only checks. Scope is `cmd_add` only -- not
`cmd_remove` / `cmd_replace` -- to keep the change minimal. Other
membership-mutating commands can adopt the same helper later if a similar
gap is found there.

No new flag (`--force-bootstrap` etc.). Stale-pool.json recovery already
has documented paths: `braid discover`, `braid remove-missing`, or manual
deletion of `/var/lib/braid/pool.json`. The error message points at them.

## Changes

### 1. New preflight helper -- `cli/src/preflight.rs`

Add:

```rust
pub fn check_pool_unlocked_if_membership_exists(
    membership: &PoolMembership,
    pool: &PoolState,
) -> Result<(), String> {
    if pool.mounted || membership.disks.is_empty() {
        return Ok(());
    }
    let n = membership.disks.len();
    let names: Vec<&str> = membership.disks.keys().map(String::as_str).collect();
    Err(format!(
        "pool exists but is not unlocked -- pool.json lists {n} member(s): {}.\n\
         Run `braid unlock` first, then re-run `braid add`.\n\
         If pool.json is stale (members no longer plugged in or you intend \
         to start over), reconcile with `braid discover` / `braid remove-missing`, \
         or remove /var/lib/braid/pool.json manually.",
        names.join(", ")
    ))
}
```

Notes:

- ASCII only, `--` not em-dash, per the CLI Output Style rule in AGENTS.md.
- Returns `Result<(), String>` to match the surrounding helpers
  (`check_no_pending_operation`, `check_ups_not_on_battery`).
- Listing the member names makes the error self-explanatory and catches
  cases where the pool the user *thinks* is loaded is actually a different
  one.

### 2. Wire it into `plan_add` -- `cli/src/add.rs`

Insert one call between the pool probe (currently ends at line 744) and the
existing `if pool.mounted { ... }` block (line 746). The new check uses
already-loaded `pool_membership` and freshly-probed `pool`, so it costs no
extra I/O.

```rust
// (existing) probe pool
let pool = match probe_pool(...) { ... };

// NEW: refuse if pool.json lists members but pool isn't unlocked.
// Catches the silent-bootstrap case where a fresh disk + locked pool
// would otherwise overwrite pool.json with a one-disk pool.
if let Err(msg) =
    preflight::check_pool_unlocked_if_membership_exists(&pool_membership, &pool)
{
    return AddPlanReport {
        notes,
        result: Err(AddError::Validation(msg)),
    };
}

if pool.mounted {
    ...mutation preflight...
}
```

The new check sits *after* `check_no_pending_operation` (line 648), so a
pending-op error still wins -- pending-op tells the operator about an
in-flight, half-finished operation, which is the more urgent signal. The
locked-pool refusal is for the steady-state "you forgot to unlock" case.

`notes` is empty at this point in the function (no preflight has populated
it yet), so the report shape matches the empty-notes pattern used by the
checks above it. Even so, returning via `AddPlanReport { notes, result }`
keeps the contract uniform and avoids surprises if a future preflight is
inserted earlier.

### 3. Test coverage

#### Rust unit tests

Two layers, both required.

**Helper unit test in `cli/src/preflight.rs`** -- behaviour table on
`check_pool_unlocked_if_membership_exists`:

- empty membership + unmounted pool -> `Ok`
- empty membership + mounted pool -> `Ok`
- non-empty membership + mounted pool -> `Ok`
- non-empty membership + unmounted pool -> `Err`, message contains
  `not unlocked` and the disk name(s)

**`plan_add` precedence test in `cli/src/add.rs`** -- gates the claim
that `check_no_pending_operation` wins over the new locked-pool refusal
(see "Wire it into `plan_add`" above). Per
`feedback_gate_tests_exhaustive_matrix_at_seam.md`, both branches need a
test that distinguishes plausible wrong orderings.

Reuses the existing `plan_add_fixture` test infra at
`cli/src/add.rs:3210`. Two cases:

1. `pool.json` non-empty + `pending-op.json` present + pool unmounted ->
   `plan_add` returns `Err`; message contains `interrupted operation
   detected`. (Fails if the new check were placed before
   `check_no_pending_operation`.)
2. `pool.json` non-empty + no `pending-op.json` + pool unmounted ->
   `plan_add` returns `Err`; message contains `not unlocked`. (Fails if
   the new check were missing or unwired.)

These together pin the ordering at the seam, not just one side of it.

#### NixOS VM test -- new `tests/module/add-locked-pool.{nix,py}`

This is the primary behavioural test. Pattern copied from
`tests/module/raid1.nix` (uses `tests/module/lib/initrd-fixture.nix` to
pre-format LUKS+btrfs RAID1 in the initrd, leaves all mappers closed at
boot, so the pool comes up locked).

Setup:

- 3 empty virtual disks: `disk1`, `disk2`, `disk3`.
- `initrd-fixture` formats `disk1`+`disk2` as LUKS+btrfs RAID1.
- `disk3` is left bare.
- `pool.json` is seeded via `systemd.tmpfiles.rules` listing `disk1` +
  `disk2` (pattern from
  `tests/module/auto-unlock-key-present.nix:65-68`).
- `braid` module enabled with `package = braid` (use
  `linuxCrane.braid-cli-unwrapped` per the module-test convention in
  `flake.nix:325-330`).

Assertions in `add-locked-pool.py`. Preamble: `#`-style block following
Intent / Why it exists / Scenario per AGENTS.md "Test Conventions". The
`add` invocation must use the canonical `add_cmd` shape from
`tests/cli/braid-add-disk.py:25-32` so the real-run case actually
exercises the destructive path -- without `--yes --passphrase-stdin`,
`braid add` aborts at the confirmation prompt before reaching LUKS
format, and a missing-guard build would still appear "safe" for the
wrong reason.

```python
import shlex
passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"

def add_cmd(key, *, dry_run=False):
    pq = shlex.quote(passphrase)
    flags = "--passphrase-stdin --yes" + (" --dry-run" if dry_run else "")
    return (
        f"printf '%s\\n' {pq} | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add {key}=/dev/disk/by-id/virtio-{key} {flags}"
    )
```

1. **Locked pool + fresh disk + dry-run**:
   - run `add_cmd("disk3", dry_run=True)` -> `machine.fail(...)`,
     captured stderr+stdout
   - combined output contains `not unlocked` and `disk1` and `disk2`
   - combined output contains no `mkfs.btrfs`, no `mount`, no
     `cryptsetup luksFormat` (sanity: no plan rendered)
   - `cat /var/lib/braid/pool.json` byte-identical to the seeded
     contents
   - `cryptsetup isLuks /dev/disk/by-id/virtio-disk3` fails (disk3
     still bare)

2. **Locked pool + fresh disk + real run** (the destructive path that
   the helper must guard):
   - run `add_cmd("disk3")` -> `machine.fail(...)`
   - same `not unlocked` assertion
   - `pool.json` byte-identical to seeded contents
   - `cryptsetup isLuks /dev/disk/by-id/virtio-disk3` fails
   - `/dev/mapper/braid-disk3` does not exist
   - `findmnt /mnt/storage` fails (pool still not mounted)

3. **Sanity: unlock then add**:
   - `printf '%s\n' testpassphrase | braid unlock --passphrase-stdin`
   - `add_cmd("disk3")` -> `machine.succeed(...)`
   - `btrfs filesystem show /mnt/storage` lists 3 devices
   - `pool.json` now lists `disk1`, `disk2`, `disk3`

#### Register in `flake.nix`

After the `braid-module-add-bootstrap` entry at `flake.nix:325-329`, add:

```nix
braid-module-add-locked-pool = pkgs.testers.nixosTest (
  import ./tests/module/add-locked-pool.nix {
    braid = linuxCrane.braid-cli-unwrapped;
  }
);
```

Per `feedback_new_vm_test_must_register_in_flake.md`, this is the only
registration needed -- `just test-vm` dispatches via `checks` entries in
`flake.nix`.

## Files modified

- `cli/src/preflight.rs` -- add `check_pool_unlocked_if_membership_exists`
  + unit test
- `cli/src/add.rs` -- one new preflight call in `plan_add` after the pool
  probe (around line 744)
- `tests/module/add-locked-pool.nix` -- new (modeled on `raid1.nix`)
- `tests/module/add-locked-pool.py` -- new
- `flake.nix` -- register `braid-module-add-locked-pool`

## Files NOT modified

- `cli/src/probe.rs` -- the existing `PresentLuks` / `PresentNotLuks`
  classification is fine. The bug is not in the disk probe; it is in the
  pool-state branch in `plan_add` / `compile_add_steps_multi`.
- `cli/src/membership.rs` -- `PoolMembership::empty()` and `disks` map are
  the right shape already.
- `cli/src/preview.rs` / dry-run handling -- the new error returns
  `AddError::Validation` and goes through the existing `AddPlanReport`
  failure path. `--dry-run` already surfaces validation errors before
  rendering a plan, so no preview changes needed. The VM test verifies
  this end-to-end.

## Edge cases (decisions encoded above)

- **Stale pool.json (members refer to disks no longer plugged in)**:
  same path -- still refuse. Error message points at `braid discover` /
  `braid remove-missing` / manual delete. No `--force-bootstrap` flag.
- **Pending-op + locked pool**: pending-op error wins (existing ordering;
  `check_no_pending_operation` runs at line 648, well before the new
  check).
- **Empty pool.json + locked / unmounted state**: not refused (pool is a
  legitimate fresh-bootstrap target). Existing behaviour preserved.
- **Mounted pool with no membership** (shouldn't normally happen, but is
  the inverse symmetry case): not refused. Existing behaviour preserved.

## Verification

- `cargo test -p braid-cli preflight::` -- unit test passes
- `just test-vm braid-module-add-locked-pool` -- VM test passes for all
  three scenarios
- `just test-vm braid-module-add-bootstrap` -- existing bootstrap test
  still passes (sanity that we didn't break the fresh-bootstrap path,
  since that test seeds no pool.json and so `pool_membership.disks` is
  empty)
- `just test-vm braid-add-disk` -- existing add lifecycle test still
  passes (it bootstraps from empty pool.json, so the new check is a
  no-op on its path)

## Risks / drawbacks

- A user with a legitimately stale pool.json (pool was destroyed
  out-of-band, etc.) now needs to take an explicit step to re-bootstrap
  rather than just running `braid add`. This is the correct trade-off:
  the cost of one extra command is much lower than the cost of silently
  destroying two disks of data. The error message documents the
  remediation in-line.
