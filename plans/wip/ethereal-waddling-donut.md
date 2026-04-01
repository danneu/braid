# Plan: Extract shared LUKS identity validation for `braid add`

## Context

`cmd_add` and `compile_add_steps_multi` in `cli/src/add.rs` both implement the same identity classification flow for `PresentLuks` disks:
1. Read the LUKS label and reject non-braid labels
2. Reject if pool is not mounted
3. Call `classify_braid_disk_fsid` and handle all enum variants

The duplication has already caused error message drift: `BraidLabeledNoBtrfs` produces different text in each path. The fix is two focused shared helpers that both paths call, leaving no duplicated error text.

## Critical file

- `cli/src/add.rs`

## Approach

Two helpers, each with a single responsibility:

- **`validate_braid_preconditions`** — side-effect-free; reads label + checks pool mounted; called by both paths for every `PresentLuks` disk (including the `!mapper_open` deferred case)
- **`identity_to_error`** — maps `AddLuksIdentity` error variants to canonical `AddError`; called by both paths after `classify_braid_disk_fsid`

No new action enum. Existing control flow is mostly unchanged.

## Changes

### 1. Add `validate_braid_preconditions` (after `read_luks_label`)

```rust
/// Validate the preconditions for adding a PresentLuks disk.
/// Reads the LUKS label and checks the pool is mounted.
/// No side effects — works on the raw device, no mapper required.
fn validate_braid_preconditions<R: CommandRunner>(
    runner: &R,
    name: &str,
    device: &str,
    pool: &PoolState,
) -> Result<(), AddError> {
    let label = read_luks_label(runner, device)?;
    let expected_label = format!("braid-{name}");
    if label.as_deref() != Some(expected_label.as_str()) {
        return Err(AddError::Validation(format!(
            "disk '{}' ({}) is already a LUKS container but is not labeled as {}; \
             braid will not adopt a non-braid encrypted device",
            name, device, expected_label,
        )));
    }
    if !pool.mounted {
        return Err(AddError::Validation(format!(
            "disk '{}' is braid-labeled but no mounted pool exists to verify identity; \
             bootstrap only accepts fresh disks",
            name,
        )));
    }
    Ok(())
}
```

### 2. Add `identity_to_error` (after `classify_braid_disk_fsid`)

Maps only the error variants. Returns `None` for `AlreadyInPool` and `Recoverable` (successes, handled inline by callers).

```rust
/// Map an AddLuksIdentity error variant to a canonical AddError.
/// Returns None for non-error outcomes (AlreadyInPool, Recoverable).
fn identity_to_error(identity: &AddLuksIdentity, name: &str) -> Option<AddError> {
    match identity {
        AddLuksIdentity::BraidLabeledNoBtrfs => Some(AddError::Validation(format!(
            "disk '{}' is braid-labeled but contains no btrfs superblock; \
             identity is ambiguous, so braid will not re-add it automatically. \
             Wipe the disk and add it again as fresh.",
            name,
        ))),
        AddLuksIdentity::BraidLabeledForeignPool => Some(AddError::Validation(format!(
            "disk '{}' is a braid-managed device from a different btrfs filesystem; \
             braid will not merge foreign pools",
            name,
        ))),
        _ => None,
    }
}
```

### 3. Update `cmd_add` Pass 1 (replace lines ~346–415)

```rust
let mut luks_guard = LuksCleanupGuard::new(runner);
let mut needs_pool_add: Vec<usize> = Vec::new();

for (i, p) in probed.iter().enumerate() {
    let ConfigDiskState::PresentLuks { mapper_open, .. } = &p.state else {
        continue;
    };
    let name = names[i];
    let by_id = by_ids[i];
    let mn = mapper_name(name);

    validate_braid_preconditions(runner, name, &by_id.0, &pool)?;

    if !mapper_open {
        ensure_luks_open(runner, fs, name, by_id, &passphrase)?;
        luks_guard.track(mn.0.clone());
        eprintln!("LUKS opened: {} → {}", by_id, mn);
    }

    let identity = classify_braid_disk_fsid(runner, name, &mn, &pool)?;
    if let Some(err) = identity_to_error(&identity, name) {
        return Err(err);
    }
    match identity {
        AddLuksIdentity::BraidLabeledAlreadyInPool => continue,
        AddLuksIdentity::BraidLabeledRecoverable => {
            eprintln!(
                "note: braid-labeled disk '{}' verified as pool member. \
                 Completing recovery add.",
                name
            );
            needs_pool_add.push(i);
        }
        _ => unreachable!("error variants handled by identity_to_error above"),
    }
}
```

### 4. Update `compile_add_steps_multi` PresentLuks arm (replace lines ~598–665)

```rust
ConfigDiskState::PresentLuks { mapper_open, .. } => {
    // Preconditions always checked — no mapper required.
    validate_braid_preconditions(runner, name, &by_id.0, pool)?;

    if *mapper_open {
        let identity = classify_braid_disk_fsid(runner, name, &mn, pool)?;
        if let Some(err) = identity_to_error(&identity, name) {
            return Err(err);
        }
        match identity {
            AddLuksIdentity::BraidLabeledAlreadyInPool => continue,
            AddLuksIdentity::BraidLabeledRecoverable => {
                steps.push(AddStep {
                    risk: "safe",
                    description: format!(
                        "btrfs device add /dev/mapper/{} {} (recovery)",
                        mn, mount_point
                    ),
                });
                needs_pool_add += 1;
            }
            _ => unreachable!("error variants handled by identity_to_error above"),
        }
    } else {
        // Mapper closed — FSID verification deferred to execution time.
        steps.push(AddStep {
            risk: "safe",
            description: format!(
                "LUKS open + identity verification at execution time → {}",
                mn
            ),
        });
        needs_pool_add += 1;
    }
}
```

### 5. Add regression tests for canonical messages

Add to the existing `#[cfg(test)]` block in `add.rs`.

**Helper tests** pin the canonical message text so `validate_braid_preconditions` and `identity_to_error` can't drift:

```rust
#[test]
fn preconditions_non_braid_label_canonical_message() {
    // validate_braid_preconditions produces the canonical label-mismatch error.
    let runner = MockRunner::default().with_output(
        CmdRequest::CryptsetupLuksDumpText { device: "/dev/disk/by-id/disk1".into() },
        luks_dump_text_with_label("some-other-label"),
    );
    let pool = pool_unmounted();
    let err = validate_braid_preconditions(&runner, "disk1", "/dev/disk/by-id/disk1", &pool)
        .unwrap_err()
        .to_string();
    assert!(err.contains("not labeled as braid-disk1"), "got: {err}");
    assert!(err.contains("braid will not adopt a non-braid encrypted device"), "got: {err}");
}

#[test]
fn preconditions_no_pool_canonical_message() {
    // validate_braid_preconditions produces the canonical no-mounted-pool error.
    let runner = MockRunner::default().with_output(
        CmdRequest::CryptsetupLuksDumpText { device: "/dev/disk/by-id/disk1".into() },
        luks_dump_text_with_label("braid-disk1"),
    );
    let pool = pool_unmounted();
    let err = validate_braid_preconditions(&runner, "disk1", "/dev/disk/by-id/disk1", &pool)
        .unwrap_err()
        .to_string();
    assert!(err.contains("no mounted pool exists to verify identity"), "got: {err}");
    assert!(err.contains("bootstrap only accepts fresh disks"), "got: {err}");
}

#[test]
fn identity_to_error_no_btrfs_canonical_message() {
    // identity_to_error produces the canonical BraidLabeledNoBtrfs error.
    let err = identity_to_error(&AddLuksIdentity::BraidLabeledNoBtrfs, "disk1")
        .unwrap()
        .to_string();
    assert!(err.contains("contains no btrfs superblock"), "got: {err}");
    assert!(err.contains("identity is ambiguous"), "got: {err}");
    assert!(err.contains("Wipe the disk and add it again as fresh"), "got: {err}");
}

#[test]
fn identity_to_error_foreign_pool_canonical_message() {
    // identity_to_error produces the canonical BraidLabeledForeignPool error.
    let err = identity_to_error(&AddLuksIdentity::BraidLabeledForeignPool, "disk1")
        .unwrap()
        .to_string();
    assert!(err.contains("different btrfs filesystem"), "got: {err}");
    assert!(err.contains("braid will not merge foreign pools"), "got: {err}");
}

#[test]
fn identity_to_error_success_variants_return_none() {
    // identity_to_error returns None for non-error outcomes.
    assert!(identity_to_error(&AddLuksIdentity::BraidLabeledAlreadyInPool, "disk1").is_none());
    assert!(identity_to_error(&AddLuksIdentity::BraidLabeledRecoverable, "disk1").is_none());
}
```

**Parity test** asserts that the dry-run path (`compile_add_steps_multi`) produces exactly the same `BraidLabeledNoBtrfs` error as the shared `identity_to_error` function that the execution path calls — pinning both callers to the shared implementation:

```rust
#[test]
fn dry_run_and_execution_produce_same_no_btrfs_error() {
    // dry-run path: compile_add_steps_multi with mapper_open=true
    let runner = MockRunner::default()
        .with_output(
            CmdRequest::CryptsetupLuksDumpText { device: "/dev/disk/by-id/disk1".into() },
            luks_dump_text_with_label("braid-disk1"),
        )
        .with_output(
            CmdRequest::BtrfsFilesystemShowTarget { target: "/dev/mapper/braid-disk1".into() },
            btrfs_show_no_btrfs(),
        );
    let probed = vec![probed_present_luks("disk1", true)];
    let pool = pool_mounted_with_fsid("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");

    let dry_err = compile_add_steps_multi(
        &runner,
        &["disk1"],
        &[&ByIdPath("/dev/disk/by-id/disk1".into())],
        &probed,
        &pool,
        &MountPoint("/mnt/storage".into()),
    )
    .unwrap_err()
    .to_string();

    // execution path: identity_to_error is the shared function cmd_add calls
    let exec_err = identity_to_error(&AddLuksIdentity::BraidLabeledNoBtrfs, "disk1")
        .unwrap()
        .to_string();

    assert_eq!(
        dry_err, exec_err,
        "dry-run and execution paths must produce identical BraidLabeledNoBtrfs error"
    );
}
```

## Verification

```
just test-rust
```

The structural guarantee: both `cmd_add` and `compile_add_steps_multi` now call `validate_braid_preconditions` and `identity_to_error` — the same functions. Message drift between the two paths is impossible without changing a single function. The new tests pin the canonical message text for each error case.
