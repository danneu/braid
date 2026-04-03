# Extract policy-based exclusive-op handling + `require_exclusive_access`

## Context

`cmd_add`, `cmd_remove`, `cmd_remove_missing`, `cmd_replace`, and `cmd_lock` all manually match on `check_no_exclusive_op` results with command-specific policy logic. The mutating commands (add/remove/remove-missing/replace) share identical handling; lock differs by rejecting any busy op. Extracting an `ExclusiveOpPolicy` enum and `check_exclusive_op_with_policy` helper eliminates duplication and names the two distinct policies.

## Changes (implemented)

### 1. `ExclusiveOpPolicy` enum + `check_exclusive_op_with_policy` in `cli/src/preflight.rs`

- `RejectAnyBusy` — lock's policy: hard-fail on any active exclusive op.
- `RejectPausedBalanceElseEnqueue` — mutating command policy: reject paused balance, warn-and-proceed on anything else.
- `check_exclusive_op_with_policy(fs, fsid, policy)` → `Result<(), String>` dispatches on both.

### 2. `require_exclusive_access` in `cli/src/preflight.rs`

Composes `check_exclusive_op_with_policy(RejectPausedBalanceElseEnqueue)` + `check_not_read_only`.

### 3. Callers updated

| File | Change |
|------|--------|
| `cli/src/add.rs` | 15-line block → `require_exclusive_access` |
| `cli/src/remove.rs` | same |
| `cli/src/remove_missing.rs` | same |
| `cli/src/replace.rs` | same |
| `cli/src/lock.rs` | 8-line block → `check_exclusive_op_with_policy(RejectAnyBusy)` |

### 4. Tests added in `cli/src/preflight.rs`

**Policy tests** (5):
- `policy_reject_any_passes_when_none`
- `policy_reject_any_rejects_busy_op`
- `policy_reject_any_rejects_balance_paused`
- `policy_enqueue_proceeds_on_busy_op`
- `policy_enqueue_rejects_balance_paused`

**`require_exclusive_access` tests** (4):
- `require_exclusive_access_passes_when_none`
- `require_exclusive_access_rejects_balance_paused`
- `require_exclusive_access_proceeds_on_busy_op`
- `require_exclusive_access_rejects_read_only`

## Verification

All 640 tests pass: `cargo test -p braid-cli`
