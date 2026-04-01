# Plan: Warn about paused balance in `braid recover`

## Context

After a crash during `braid add`'s RAID1 balance phase, `braid recover` correctly rebuilds `pool.json` from the live pool. But because braid always mounts with `skip_balance`, the interrupted balance stays paused. The user sees "Recovery complete." and has no indication that RAID1 conversion is incomplete — some chunks may be single-profile with no redundancy.

`braid unlock` already has this warning (unlock.rs:77-88) but as inline code. Since recover needs the same check, extract shared helpers and have both call them.

## Changes

### 1. Add shared helpers in `cli/src/status.rs`

Add next to the existing `get_balance_report()` (status.rs:618):

**`paused_balance_warning()`** — computes the message:
```rust
pub fn paused_balance_warning<R: CommandRunner>(runner: &R, mount_point: &str) -> Option<String> {
    match get_balance_report(runner, mount_point) {
        BalanceReport::Paused { .. } => Some(format!(
            "paused balance detected \u{2014} will not auto-resume\n  \
             resume:  btrfs balance resume {mount_point}\n  \
             cancel:  btrfs balance cancel {mount_point}"
        )),
        _ => None,
    }
}
```

**`emit_paused_balance_warning()`** — checks + emits to a writer:
```rust
pub fn emit_paused_balance_warning<R: CommandRunner>(
    runner: &R,
    mount_point: &str,
    out: &mut dyn std::io::Write,
) -> bool {
    if let Some(warning) = paused_balance_warning(runner, mount_point) {
        writeln!(out).ok();
        for line in warning.lines() {
            writeln!(out, "  {line}").ok();
        }
        true
    } else {
        false
    }
}
```

Unit tests in status.rs (next to existing `get_balance_report` tests at ~line 2243):

```rust
#[test]
fn paused_balance_warning_returns_message_when_paused() {
    // mock BtrfsBalanceStatus → paused
    let warning = paused_balance_warning(&runner, "/mnt/storage");
    assert!(warning.is_some());
    let msg = warning.unwrap();
    assert!(msg.contains("paused balance"));
    assert!(msg.contains("resume"));
    assert!(msg.contains("cancel"));
}

#[test]
fn paused_balance_warning_returns_none_when_idle() {
    // mock BtrfsBalanceStatus → no balance
    assert!(paused_balance_warning(&runner, "/mnt/storage").is_none());
}

#[test]
fn emit_paused_balance_warning_writes_to_buffer() {
    // mock BtrfsBalanceStatus → paused
    let mut buf = Vec::new();
    let warned = emit_paused_balance_warning(&runner, "/mnt/storage", &mut buf);
    assert!(warned);
    let output = String::from_utf8(buf).unwrap();
    assert!(output.contains("paused balance"));
    assert!(output.contains("resume"));
    assert!(output.contains("cancel"));
}

#[test]
fn emit_paused_balance_warning_silent_when_idle() {
    // mock BtrfsBalanceStatus → no balance
    let mut buf = Vec::new();
    let warned = emit_paused_balance_warning(&runner, "/mnt/storage", &mut buf);
    assert!(!warned);
    assert!(buf.is_empty());
}
```

### 2. Refactor `cmd_unlock` to call shared helper (`cli/src/unlock.rs`)

Replace lines 71-88 (inline `tag()` + match block) with one line:

```rust
crate::status::emit_paused_balance_warning(runner, mount_point.as_str(), &mut std::io::stderr());
```

Drop the local `tag()` function. The `unlock_warns_on_paused_balance` test (line 558) still passes — it asserts `Ok(())` and the mock is still consumed by the helper. The visual change (`[warn]  ...` → `  paused balance detected...`) is acceptable since braid is unreleased and aligns both commands to one format.

### 3. Add balance warning to `cmd_recover` (`cli/src/recover.rs`)

No return type change — stays `Result<(), RecoverError>`. After line 160 (`"pending-op.json cleared. Recovery complete."`), add one line:

```rust
crate::status::emit_paused_balance_warning(runner, mount_point, &mut std::io::stderr());
```

No separate integration test for this call. The emit helper is thoroughly tested in status.rs with a buffer writer, and the call site is a single line verifiable by review.

### 4. Update `README.md`

**Recovery section (line 201):** Append to the `pending-op.json` description:

> If a paused balance is detected (e.g. from an interrupted RAID1 conversion), `recover` warns and tells you to resume or cancel it manually.

**Paused-balance example (lines 287-293):** Update the existing code block to match the new shared format. Current:

```
[warn]  paused balance detected — will not auto-resume
           resume:  btrfs balance resume /mnt/storage
           cancel:  btrfs balance cancel /mnt/storage
```

Updated to match `emit_paused_balance_warning` output:

```
  paused balance detected — will not auto-resume
    resume:  btrfs balance resume /mnt/storage
    cancel:  btrfs balance cancel /mnt/storage
```

## Files modified

| File | Change |
|------|--------|
| `cli/src/status.rs` | Add `paused_balance_warning()` + `emit_paused_balance_warning()` + 4 unit tests |
| `cli/src/unlock.rs` | Replace inline balance check with shared helper call (one line) |
| `cli/src/recover.rs` | Call shared helper after recovery (one line) |
| `README.md` | Update recovery section + paused-balance example to match new format |

## Verification

- `just test-rust` — all unit tests pass (existing + 4 new helper tests)
