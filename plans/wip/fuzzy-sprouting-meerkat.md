# Fix enroll_key_file backup mock mismatches

## Context

The atomic-write change to `backup_luks_header_to` (luks.rs:110-130) now writes to `{mapper}.luksheader.tmp` and renames to `{mapper}.luksheader`. Two unit tests in `enroll_key_file.rs` fail because their mocks don't account for this.

## Which tests failed and why

**`apply_enrolls_needs_enroll_items`** (enroll_key_file.rs:738) — mocks register `CryptsetupLuksHeaderBackup` with path `braid-disk1.luksheader` and `braid-disk2.luksheader`. The code now passes `braid-disk1.luksheader.tmp` / `braid-disk2.luksheader.tmp`. No mock matches → `MissingMock` error at line 791.

**`apply_mixed_plan`** (enroll_key_file.rs:822) — same issue for `braid-disk2.luksheader` vs `braid-disk2.luksheader.tmp`. Fails at line 861.

## Why not just pre-create .tmp files?

`backup_luks_header_to` explicitly deletes any existing `.tmp` file (luks.rs:113-114) before invoking cryptsetup, then calls `set_permissions` and `rename` on the path cryptsetup is supposed to create. Pre-creating `.tmp` files would just get deleted, and the subsequent `set_permissions` would ENOENT.

## Fix

Make MockRunner create the file on disk when it successfully handles a `CryptsetupLuksHeaderBackup` request — mirroring what real cryptsetup does. This is the right abstraction level: the mock models the side effect of the command it's mocking, so tests don't need to hand-manage temp files.

### Changes

**`cli/src/cmd.rs` — `MockRunner::run`** (line 686):
After looking up the mock output, if the request is `CryptsetupLuksHeaderBackup` and exit_status is 0, create the `backup_path` file (empty). This keeps the side-effect modeling in the mock rather than scattered across every test.

```rust
fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
    let output = self.outputs
        .get(&format!("{request:?}"))
        .cloned()
        .ok_or(CmdError::MissingMock)?;
    // Model cryptsetup's side effect: it creates the backup file on success
    if let CmdRequest::CryptsetupLuksHeaderBackup { backup_path, .. } = request {
        if output.exit_status == 0 {
            if let Some(parent) = std::path::Path::new(backup_path).parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| CmdError::Failed(format!("mock: create_dir_all: {e}")))?;
            }
            std::fs::write(backup_path, b"")
                .map_err(|e| CmdError::Failed(format!("mock: write backup: {e}")))?;
        }
    }
    Ok(output)
}
```

`CmdError::Failed(String)` already exists (cmd.rs:568) — no new variant needed.

**`cli/src/cmd.rs` — add unit test** in `mod tests`: assert that a successful mocked `CryptsetupLuksHeaderBackup` creates the requested `backup_path` file, and that a failed one (exit_status != 0) does not.

**`cli/src/enroll_key_file.rs` — test mocks** (lines 761, 772, 842):
Change `.luksheader"` → `.luksheader.tmp"` in `CryptsetupLuksHeaderBackup` mock paths to match what the code now passes.

**`cli/src/enroll_key_file.rs` — remove pre-creates** (lines 747-748, 831):
Remove the manual `std::fs::write(...luksheader...)` pre-creates — MockRunner now handles this.

## Verification

`just test-rust`
