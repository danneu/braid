# Plan: RAII cleanup guard for LUKS mappers in `braid add`

## Context

`cmd_add` opens LUKS mappers via `ensure_luks_open` but has no cleanup on
error paths. If a later step in the LUKS phase fails (identity check, keyfile
enrollment, second disk's format), the mapper stays open, blocking
`wipefs`/`cryptsetup` on the underlying device. The user must manually
`cryptsetup close` to recover.

Fix: an RAII drop guard scoped to the LUKS-open/identity-verification phase
only. Once the loop completes and pool mutation begins, the mappers are
committed — `cryptsetup close` would fail busy at that point anyway, so
the guard disarms before pool operations.

## File to modify

`cli/src/add.rs` — guard struct + wiring + tests. Nothing else changes.

## Implementation

### 1. `LuksCleanupGuard` struct (private to `add.rs`)

Place above `cmd_add`.

```rust
struct LuksCleanupGuard<'a, R: CommandRunner> {
    runner: &'a R,
    mappers: Vec<String>,
    armed: bool,
}
```

- `new(runner) -> Self` — empty, armed
- `track(&mut self, mapper: String)` — record a mapper we opened
- `disarm(&mut self)` — set `armed = false`
- `Drop`: if armed, iterate `mappers` in reverse, `CryptsetupClose` each.
  Best-effort — log warnings, never panic. No retry (unlike `lock.rs`,
  there's no "just unmounted" race here).

### 2. Wire into `cmd_add` — three touch points

**A. Create guard** — line 269, before the LUKS phase loop:
```rust
let mut luks_guard = LuksCleanupGuard::new(runner);
```

**B. Track after each `ensure_luks_open`** that actually opens a mapper:

- Line 289 (`PresentNotLuks`): after open succeeds, `luks_guard.track(mn.0.clone());`
- Line 323 (`PresentLuks`, `!mapper_open`): after open succeeds, `luks_guard.track(mn.0.clone());`

Do NOT track in the `mapper_open == true` branch — we didn't open it.

**C. Disarm after the LUKS loop** — line 363, immediately after `}` closing
the loop and before the `needs_pool_add.is_empty()` check:
```rust
luks_guard.disarm();
```

This means:
- Any `?` or `return Err(...)` **inside the loop** triggers cleanup (correct)
- The `needs_pool_add.is_empty()` early success return at line 365 runs
  after disarm — no spurious cleanup (correct)
- Pool operations (bootstrap, device add, balance) run after disarm — no
  futile close attempts against btrfs-active mappers (correct)

### 3. Tests

**`SpyRunner`** — test-only `CommandRunner` impl (in `mod tests`) that
delegates to `MockRunner` but records `CryptsetupClose` mapper names in a
`RefCell<Vec<String>>`.

Each test must have the mandated block comment (Intent / Why it exists /
Scenario) per AGENTS.md test conventions.

**`guard_closes_on_armed_drop`**
- Intent: Drop calls `CryptsetupClose` for each tracked mapper
- Why: core correctness — without this, the guard is dead code
- Scenario: `cmd_add` opens a mapper, a later step in the LUKS phase fails,
  the guard fires on unwind and closes the mapper

**`guard_noop_when_disarmed`**
- Intent: `disarm()` prevents close on drop
- Why: successful LUKS phase must not close the mappers it just opened for
  the pool phase
- Scenario: all identity checks pass, guard is disarmed, drop is a no-op

**`preexisting_mapper_not_closed`**
- Intent: a mapper already open before `cmd_add` is not tracked or closed
- Why: closing a pre-existing mapper would break a running pool
- Scenario: `PresentLuks` with `mapper_open=true` fails identity check;
  guard must not close that mapper

## Verification

```
just test-rust
```
