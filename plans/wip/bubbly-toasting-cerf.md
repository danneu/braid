# Plan: Make `--missing-id` required for `remove-missing`

## Context

`braid remove-missing` currently accepts `--missing-id` as optional. When omitted with exactly 1 missing device, it silently proceeds against that device. This is a destructive, long-running operation on a degraded pool — the operator should always positively identify the device being evicted. Making `--missing-id` required eliminates the silent-default path.

This also simplifies the code: the `None` branch in `compile_steps` (which emits `BtrfsDeviceRemoveMissing` — the untargeted btrfs command) becomes dead code and can be removed along with `pool_remove_missing` and `CmdRequest::BtrfsDeviceRemoveMissing`.

## Changes

### 1. CLI args — `cli/src/main.rs`

- `RemoveMissingArgs.missing_id`: `Option<u64>` → `u64`
- Callsite (line ~287): pass `args.missing_id` directly (signature changes too)

### 2. Command implementation — `cli/src/remove_missing.rs`

**Function signature** (`cmd_remove_missing`, line 57):
- `missing_id: Option<u64>` → `missing_id: u64`

**Delete** the `if missing_id.is_none()` guard (lines 99-104) — now unreachable.

**Simplify all `Option` handling** to use `missing_id` directly:
- Line 106: `if let Some(devid) = missing_id` → use `missing_id` directly
- Line 130: `check_relocation_space(runner, ..., missing_id)` → `Some(missing_id)` (callee still takes `Option`)
- Lines 133-136: `will_clear_last_missing` match → `pool.missing_count == 1`
- Line 152: confirmation label → `format!("missing device entry (devid {missing_id})")`
- Line 177: `target_devid` → just `missing_id`
- Line 195: `if missing_id.is_some()` → always true, **delete** the else branch (lines 201-203 calling `pool_remove_missing`)

**`compile_steps` (line 274)**: `missing_id: Option<u64>` → `missing_id: u64`
- Delete the `else` branch (lines 292-299) that emits `BtrfsDeviceRemoveMissing`

### 3. Journal — `cli/src/journal.rs`

- `OpKind::RemoveMissing { devid: Option<u64> }` → `devid: u64`
- Update all match sites in `recover.rs`

### 4. Dead code removal — `cli/src/pool.rs`, `cli/src/cmd.rs`

Remove now-unreachable code:
- `pool_remove_missing()` in `cli/src/pool.rs` (line 159)
- `CmdRequest::BtrfsDeviceRemoveMissing` variant in `cli/src/cmd.rs` (line 77) and its `CmdArgs` arm (line 409)
- Grep for remaining references (progress.rs doc comment at line 133)

### 5. Rust unit tests — `cli/src/remove_missing.rs`

| Test | Change |
|------|--------|
| `missing_id_always_required` | **Delete** — type system enforces this; a CLI-level regression test is added in the VM tests (see §6) |
| `enospc_check_skipped_for_single_survivor` | Pass `2` (not `Option`) |
| `three_device_pool_soft_rebalance_runs` | Pass `3`; assert `BtrfsDeviceRemove` not `BtrfsDeviceRemoveMissing` |
| `three_device_two_missing_no_rebalance` | Pass `3` |
| `journal_survives_soft_balance_failure` | Pass `3` |
| `compile_steps_shows_rebalance_when_clearing_last_missing` | Pass `3` |
| `compile_steps_omits_rebalance_with_single_survivor` | Pass `3` |
| `dry_run_render_untargeted_removal_no_balance` | **Delete** — untargeted path removed |
| Recover tests (`guidance_remove_missing_*`) | Unwrap `Some(2)` → `2` |

Mock runners (`RecordingRunner`, `ThreeDeviceRunner`, `FailingSoftBalanceRunner`):
- Add `BtrfsDeviceRemove` handler
- Update `remove_done` detection to match `BtrfsDeviceRemove` instead of `BtrfsDeviceRemoveMissing`
- Remove `BtrfsDeviceRemoveMissing` handlers

### 6. NixOS VM tests

**Devid discovery helper** — use `braid status --json` (braid's own stable interface, not raw btrfs output). `AlertCause` uses `#[serde(tag = "type", rename_all = "snake_case")]`, so missing devices serialize as `{"type": "missing_device", "devid": 3}`.

```python
def get_missing_devid():
    """Get the devid of the missing device from braid status --json."""
    import json
    raw = machine.succeed("braid status --json")
    report = json.loads(raw)
    for cause in report.get("alert_causes", []):
        if cause.get("type") == "missing_device":
            return str(cause["devid"])
    raise AssertionError("No missing device in braid status:\n" + raw)
```

| File | Current | Change |
|------|---------|--------|
| `tests/cli/braid-remove-disk.py:48-50` | `remove_missing_cmd()` no `--missing-id` | Accept devid param: `f"braid remove-missing --missing-id {devid} --yes"` |
| `tests/cli/braid-remove-disk.py:92` | "no devices missing" test | Pass `--missing-id 99` (bogus devid); assert `status != 0` (still validates the error path, just hits a different validation message) |
| `tests/cli/braid-remove-disk.py:201` | remove-missing succeeds | `remove_missing_cmd(get_missing_devid())` |
| `tests/cli/braid-remove-missing-enospc.py:75` | no `--missing-id` | Add `--missing-id` via `get_missing_devid()` |
| `tests/cli/braid-remove-missing-enospc-crash.py:111` | no `--missing-id` | Add `--missing-id` via `get_missing_devid()` |
| `tests/cli/remove-missing-membership-readonly.py:72` | no `--missing-id` | Add `--missing-id` via `get_missing_devid()` |

**New subtest in `tests/cli/braid-remove-disk.py`** — CLI regression test for required `--missing-id`:

```python
with subtest("remove-missing without --missing-id is rejected by CLI"):
    (status, output) = machine.execute("braid remove-missing --yes 2>&1")
    assert status != 0, f"Expected failure, got exit 0: {output}"
    assert "missing-id" in output.lower(), f"Expected '--missing-id' in error:\n{output}"
```

This goes in `braid-remove-disk.py` after the "simulate disk3 death" phase so there's actually a missing device — ensuring the rejection is about the missing flag, not about pool state.

### 7. Documentation

**`README.md`** (lines 193-198):
- Replace both example lines with a single: `sudo braid remove-missing --missing-id 3`
- Remove "auto-detected" / "multiple missing" comments — always required now
- Keep "Use `braid status` to see device IDs."

**`docs/decisions/012-intent-cli.md`** (line 21):
- Table row: `braid remove-missing` → `braid remove-missing --missing-id <devid>`

### 8. Shell completion test — `tests/cli/shell-completion.py`

No changes needed.

## Verification

1. `cargo test -p braid-cli remove_missing` — unit tests pass
2. `cargo test -p braid-cli recover` — journal/recover tests pass
3. `cargo test -p braid-cli` — full unit test suite, no regressions
4. `just test braid-remove-disk braid-remove-missing-enospc braid-remove-missing-enospc-crash remove-missing-membership-readonly shell-completion` — VM tests pass

