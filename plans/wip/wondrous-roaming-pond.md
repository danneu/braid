# Fix: Corrupt pool.json masked as empty pool in `braid status`

## Context

`status.rs:389-391` uses `Err(_) => PoolMembership::empty()`, collapsing `NotFound` (expected — no pool yet) and `Corrupt` (file exists but unreadable/unparseable) into the same fallback. A corrupt `pool.json` silently shows zero configured disks instead of alerting the user.

The correct pattern already exists in `add.rs:217-220` and `doctor.rs:176-191`.

## Changes

### 1. Add `Membership` variant to `StatusError`

**File:** `cli/src/status.rs` (line 210, after the `Validation` variant)

```rust
#[error("{0}")]
Membership(#[from] membership::MembershipError),
```

### 2. Replace wildcard match with variant-aware match

**File:** `cli/src/status.rs` (lines 387-392)

Replace:
```rust
// 3. Membership load (try_load pattern)
let membership_result = membership::load_membership(paths);
let membership = match &membership_result {
    Ok(m) => m.clone(),
    Err(_) => PoolMembership::empty(),
};
```

With:
```rust
// 3. Membership load — NotFound is expected (no pool yet), but Corrupt must surface
let membership = match membership::load_membership(paths) {
    Ok(m) => m,
    Err(membership::MembershipError::NotFound(_)) => PoolMembership::empty(),
    Err(e) => return Err(e.into()),
};
```

Owned match (no `&`/`.clone()`) — `NotFound` → empty, anything else propagates via the new `#[from]` conversion.

### 3. Add regression test

**File:** `cli/src/status.rs` (in `#[cfg(test)] mod tests`)

Test: write invalid content to `pool.json` via `StatePaths::custom(tmpdir)`, call `cmd_status` with a mounted-pool mock, assert the result matches `StatusError::Membership(MembershipError::Corrupt(_, _))` — same variant-matching pattern used in `membership.rs` tests.

Uses existing test infrastructure: `MockRunner` with `runner_healthy_3disk_base()` responses, `MockFs` via `fs_3disk()`, `config_3disk()`, and `StatePaths::custom()`.

## Key files

- `cli/src/status.rs` — `StatusError` enum (line 200), `cmd_status` fn (line 342), tests (line 1021)
- `cli/src/membership.rs` — `MembershipError` enum (line 10), `load_membership` (line 74)
- `cli/src/state_paths.rs` — `StatePaths::custom()` (line 15)

## Verification

- `just test-rust` — compilation + all unit tests including the new regression test
- `just test` — NixOS VM integration tests still pass
