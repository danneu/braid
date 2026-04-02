# Plan: LUKS header backup improvements

## Context

braid auto-backs up LUKS headers after formatting (`add`/`replace`) to `/var/lib/braid/luks-headers/{mapper}.img`. Three problems:

1. **Stale backups** — `enroll-key` mutates keyslots without re-backing up. Restoring a stale backup silently wipes the new keyslot.
2. **Vague extension** — `.img` is ambiguous. `.luksheader` is self-documenting.
3. **Security nudge missing** — local header backups on an unencrypted boot drive are sensitive. No warning to nudge users to copy offsite.

## Changes

### 1. Migrate `.img` → `.luksheader` in `backup_luks_header()`

**File:** `cli/src/luks.rs`

- Line 109: `{mapper}.img` → `{mapper}.luksheader`
- Update doc comment (line 97)
- After successful backup, silently clean up old `.img`:
  ```rust
  let old_path = dir.join(format!("{mapper}.img"));
  if old_path.exists() {
      let _ = std::fs::remove_file(&old_path);
  }
  ```

### 2. Backup after enroll-key

**File:** `cli/src/enroll_key_file.rs`

Add imports: `crate::config::mapper_name`, `crate::luks::backup_luks_header`.

In `apply_enrollment()` (line 130), after `luks::enroll_key_file()` succeeds, backup per-disk (not batched — matches `add`/`replace` pattern, ensures partial-success leaves correct backups):

```rust
let mn = mapper_name(name);
let backup_path = backup_luks_header(runner, &disk.by_id.0, &mn.0)?;
eprintln!("LUKS header backed up: {}", backup_path.display());
```

Error propagation works — `EnrollKeyFileError` already has `Luks(#[from] LuksError)`.

### 3. Shared advisory helper (path-injectable for testing)

**File:** `cli/src/luks.rs` — add near `HEADER_BACKUP_DIR`:

```rust
/// Scan `dir` for `.luksheader` or `.img` files and return advisories.
/// Extracted so tests can pass a tempdir instead of the real path.
fn header_backup_advisories_in(dir: &Path) -> Vec<String> {
    let has_backups = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .any(|e| {
                e.path()
                    .extension()
                    .map_or(false, |ext| ext == "luksheader" || ext == "img")
            }),
        Err(_) => false,
    };
    if has_backups {
        vec![format!(
            "LUKS header backups exist in {} \
             — copy offsite and delete local copies",
            dir.display()
        )]
    } else {
        vec![]
    }
}

/// Production wrapper — scans HEADER_BACKUP_DIR.
pub fn header_backup_advisories() -> Vec<String> {
    header_backup_advisories_in(Path::new(HEADER_BACKUP_DIR))
}
```

Checks both `.luksheader` and `.img` (covers pre-migration state).

Both CLI status and TUI consume `header_backup_advisories()` — single source of truth, no drift. Tests call `header_backup_advisories_in()` with a tempdir.

### 4. Status advisory (CLI)

**File:** `cli/src/status.rs`

- Add field to `StatusReport` struct (after line 61):
  ```rust
  #[serde(skip_serializing_if = "Vec::is_empty", default)]
  pub advisories: Vec<String>,
  ```
- **Critical:** populate `advisories` inside `build_status_report()` — in ALL code paths including the not-mounted early returns (lines 279-292, 297-312) AND the mounted path (line 342-355). Compute once via `luks::header_backup_advisories()` at the top of the function, then include in every `StatusReport` construction.
- `cmd_status()` (line 358) also has its own early-return for not-mounted (lines 378-399) — populate advisories there too, same call.
- In `format_status_human()`, render at end: `warning: {advisory}` per line.

### 5. TUI advisory

**File:** `cli/src/tui/model.rs`

- Add `pub advisories: Vec<String>` to `Model` (line 100 area)
- Set in `Model::new()` via `luks::header_backup_advisories()`
- Set `vec![]` in `Model::new_demo()`

**File:** `cli/src/tui/view/mod.rs`

- In `view_data()` (line 428), compute `advisory_height` from `model.advisories.len()`
- Add `Constraint::Length(advisory_height)` between scrub `[2]` and spacer `[3]`
- Render each advisory as a yellow `Paragraph`:
  ```
  warning: LUKS header backups on local disk — copy offsite and delete
  ```

### 6. README update

**File:** `README.md`

After the "Enroll" section (~line 278), add a brief note:

- `braid enroll` automatically re-backs up LUKS headers after enrolling keyfiles
- Header backups are stored in `/var/lib/braid/luks-headers/` with `.luksheader` extension
- `braid status` and `braid tui` warn when local backups exist — copy them offsite and delete local copies
- Security note: these files can unlock your drives. Store on encrypted offline media.

## Files to modify

| File                         | Change                                                             |
| ---------------------------- | ------------------------------------------------------------------ |
| `cli/src/luks.rs`            | `.luksheader` ext, migration cleanup, `header_backup_advisories()` |
| `cli/src/enroll_key_file.rs` | Call `backup_luks_header` after enroll                             |
| `cli/src/status.rs`          | `advisories` field on `StatusReport`, populate in ALL paths        |
| `cli/src/tui/model.rs`       | `advisories` field on `Model`                                      |
| `cli/src/tui/view/mod.rs`    | Yellow warning line(s) in `view_data()` layout                     |
| `README.md`                  | Document header backup behavior and security guidance              |

## Tests (TDD — write failing tests first)

### Rust unit tests (`just test-rust`)

1. **`enroll_key_file.rs`**: Update `apply_enrolls_needs_enroll_items` and `apply_mixed_plan` tests — add `CryptsetupLuksHeaderBackup` mock expectations. Tests will fail without the mock (MockRunner returns MissingMock), confirming backup is being called.

2. **`luks.rs`**: Test `header_backup_advisories_in(dir)` with tempdir:
   - Returns empty vec when dir doesn't exist
   - Returns advisory when `.luksheader` files present
   - Returns advisory when `.img` files present (pre-migration)
   - Returns empty vec when dir exists but is empty

3. **`status.rs`**: Test that `advisories` field is populated in `StatusReport` for both mounted and not-mounted paths in `build_status_report()`.

4. **TUI snapshot tests**: Add snapshot with advisory present to verify yellow warning renders in the layout.

### Verification

```bash
just test-rust        # unit tests
just test             # NixOS VM tests (existing, ensure no regression)
```

Manual spot-checks:

- `braid enroll` → `.luksheader` file created/updated
- `braid status` (pool offline) → warning line appears
- `braid tui` → yellow warning visible
- Delete local backups → warning disappears
