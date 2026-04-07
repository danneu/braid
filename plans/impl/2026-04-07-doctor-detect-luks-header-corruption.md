# Plan: detect LUKS header corruption in `braid doctor`

## Context

Today, when a LUKS header on a pool member disk is damaged, braid gives the user no actionable signal. Three concrete failure modes exist:

- **`discover.rs:51-66`** silently skips a disk whose `cryptsetup luksDump` output fails to parse. This is correct for discover (it scans every device in `/dev/disk/by-id`, most of which are not braid disks) and should not be changed.
- **`luks.rs:230+`** surfaces a generic exit-1 from `cryptsetup open` at unlock time, with no mention of repair tooling or off-system header backups. This is the worst possible moment to discover you don't know what to do, but improving it is out of scope here.
- **`doctor.rs:176-240`** (`check_declared_disks`) loads the membership file and verifies each declared disk's by-id symlink exists as a block device, but never probes whether the LUKS header on that block device is intact. A drive with a trashed header passes this check.

The fix is to extend `check_declared_disks` so each declared disk also gets two cheap cryptsetup probes (`isLuks`, `luksDump`), with remediation messages that distinguish between "use `cryptsetup repair`" (metadata damaged) and "restore from an off-system header backup" (header unreadable).

**Important product invariant:** the local `/var/lib/braid/luks-headers/*.luksheader` files created by `add`, `replace`, and `enroll_key_file` are **not** the intended recovery target. The intended workflow is for users to export the header off-system and remove the local copy. `braid status` and the TUI already warn when local copies persist, because their continued presence on the same machine is itself a problem. `doctor` must be consistent with that posture: it must **not** treat the presence of a local `.luksheader` file as a "recovery branch" or instruct users to restore from one. doctor's recovery guidance is generic — "restore from your off-system LUKS header backup if you have one" — and never references `/var/lib/braid/luks-headers/`.

### Alignment with `docs/principles.md`

- **Principle 3 (safe-by-construction)** — doctor stays read-only. We use only the read-only cryptsetup probes (`isLuks`, `luksDump`) and emit text. We never invoke `cryptsetup repair` or `luksHeaderRestore` ourselves; the user runs those manually with full awareness of the risk. The `repair` suggestion is always paired with explicit "make a safe backup first" guidance, matching the cryptsetup project's own warning.
- **Principle 2 (CLI-owned membership)** — pool.json is consulted only as the iterator over "disks the user declared as members." The authoritative state surfaced to the user (header intact, header damaged, header unreadable) is read from the device via cryptsetup, not from pool.json. We never surface pool.json-sourced state as if it were live truth.
- **Principle 8 (test every design decision)** — both halves of the change are covered: unit tests for the message-rendering decision, and a NixOS VM test for the detection decision (the unit tests can't, by construction, prove that real cryptsetup against a real damaged header produces the expected `DiskState`). TDD order: failing tests first.
- **Cross-command consistency** — by refusing to point at local `.luksheader` files, doctor stays aligned with the warnings `status` and the TUI already emit about those same files. All three commands tell the same story.

We agreed against:

- Adding a sibling `check_luks_headers` check (failure modes are sequential — no device → can't probe header — and a sibling would either duplicate the device guard or report two findings for the same root cause).
- Touching `discover.rs`.
- Auto-invoking `cryptsetup repair` (mutates the header; cryptsetup docs require a binary backup first, and this is the user's call to make).
- Improving the unlock-time error message in the same change (separate concern, scope creep).

## Scope

Three files edited:

1. **`cli/src/doctor.rs`** — extend `check_declared_disks` with header probes. Main work.
2. **`README.md`** — one-line update to the `braid doctor` summary so the user guide reflects the new behavior (AGENTS.md requires README to stay current).
3. **`tests/cli/braid-doctor.py`** — new subtest that corrupts a LUKS header in the existing doctor VM and asserts the doctor output reports it correctly. Required by Principle 8 — see Verification.

## Design

### Refactor first, then extend

The existing `check_declared_disks` is not unit-testable past the device-existence gate: `doctor.rs:200` calls `std::fs::metadata(path).file_type().is_block_device()`, and no mock runner can fabricate a real block device in `cargo test`. To add meaningful tests for the new code we must split the check into a thin impure outer shell plus a pure inner summarizer.

New structure:

```rust
enum DiskState {
    LuksHeaderOk,
    Missing,             // std::fs::metadata returned Err
    NotBlock,            // metadata ok but not a block device
    ProbeFailed(String), // runner.run returned Err — execution failure, not corruption
    LuksHeaderUnreadable, // isLuks returned non-zero exit
    LuksHeaderDamaged,    // isLuks exit 0, luksDump returned non-zero exit
}

// Impure: touches the filesystem and the runner. Small; exercised by VM tests only.
fn classify_disk_state<R: CommandRunner>(runner: &R, path: &Path) -> DiskState { ... }

// Pure: takes pre-classified inputs and returns the final CheckResult.
// No runner, no filesystem, no StatePaths. This is what unit tests target.
fn summarize_declared_disks(
    classifications: &[(String /* name */, String /* by_id path */, DiskState)],
) -> CheckResult { ... }

fn check_declared_disks<R: CommandRunner>(ctx: &mut DoctorContext<'_, R>) -> CheckResult {
    let membership = /* existing load + skip/warn short-circuits */;
    let classifications: Vec<_> = membership.disks.iter().map(|(name, member)| {
        let state = classify_disk_state(ctx.runner, Path::new(&member.by_id.0));
        (name.clone(), member.by_id.0.clone(), state)
    }).collect();
    summarize_declared_disks(&classifications)
}
```

`summarize_declared_disks` is fully pure: it takes only the classifications, holds no references to a runner or `StatePaths`, and emits a `CheckResult`. Unit tests build the input `Vec` by hand. There is no backup-path lookup anywhere in the implementation — by design, per the product invariant in the Context section.

`classify_disk_state` stays impure and small (~20 lines). Coverage for it comes from VM tests exercising the real path — we do not try to unit-test the filesystem gate.

The `Luks*` prefix on the header variants makes it visually obvious which variants describe LUKS-header conditions versus the prior, non-LUKS conditions (`Missing`, `NotBlock`, `ProbeFailed`).

### Probe sequence inside `classify_disk_state`

1. `std::fs::metadata(path)`
   - `Err` → `DiskState::Missing`, return.
   - `Ok` but not `is_block_device()` → `DiskState::NotBlock`, return.
2. `runner.run(CmdRequest::CryptsetupIsLuks { device })` (already in `cmd.rs:68`)
   - `Err(e)` → `DiskState::ProbeFailed(e.to_string())`, return. **This is critical: `Err` from the runner means the command couldn't execute (missing binary, IPC failure, etc.), not that cryptsetup inspected the device and found damage. Conflating the two would tell users to repair or restore a healthy disk.**
   - `Ok(raw)` with `exit_status != 0` → `DiskState::LuksHeaderUnreadable`, return.
   - `Ok(raw)` with `exit_status == 0` → proceed.
3. `runner.run(CmdRequest::CryptsetupLuksDumpText { device })` (already in `cmd.rs:69`)
   - `Err(e)` → `DiskState::ProbeFailed(e.to_string())`, return.
   - `Ok(raw)` with `exit_status != 0` → `DiskState::LuksHeaderDamaged`.
   - `Ok(raw)` with `exit_status == 0` → `DiskState::LuksHeaderOk`.

We only care about exit statuses and `Err` vs `Ok`; no parsing of `luksDump` output is needed.

### Aggregation rules in `summarize_declared_disks`

Status:

- All disks in `LuksHeaderOk` → `CheckStatus::Ok` with the existing "all N declared disk(s) present" message.
- Any disk in any other state → `CheckStatus::Warn`. Consistent with the existing check, which already uses `Warn` for missing/non-block.

Message: preserve the existing "X/Y disk(s) have problems: <parts joined by `; `>" shape, extending `parts` with new segments per category. Per-disk remediation lives inside each segment so the user sees the specific command to run.

Per-category rendering — **all messages are generic and never reference local `/var/lib/braid/luks-headers/` files:**

- **`Missing`** and **`NotBlock`**: unchanged from today's messages.
- **`ProbeFailed(err)`**: `"could not probe LUKS header on <name> (<path>): <err>"`. No repair/restore suggestion — this is tooling/execution trouble, not corruption.
- **`LuksHeaderDamaged`**: `"<name> (<path>): LUKS header metadata damaged. To attempt repair manually: cryptsetup repair --type luks2 <path> — make a safe backup of the device header before running repair."`
- **`LuksHeaderUnreadable`**: `"<name> (<path>): LUKS header unreadable. Restore from your off-system LUKS header backup if you have one (cryptsetup luksHeaderRestore). Without an off-system backup, recovery may be limited or impossible."`

There is no `Path::exists` lookup, no branching on whether a local `.luksheader` file is present, and no path string from `StatePaths::luks_headers_dir()` anywhere in the rendered output. The same message is shown regardless of what the local `/var/lib/braid/luks-headers/` directory contains.

### Things deliberately NOT done

- No new `CmdRequest` variants — both probes already exist in `cmd.rs`.
- No new public functions on `StatePaths` or `luks.rs`.
- **No reference to `/var/lib/braid/luks-headers/` in any doctor output, code, or test.** This is consistent with `status` and the TUI, which already warn about persistent local copies of those files.
- No parsing of `luksDump` output.
- No changes to `discover.rs`, `luks.rs`, `state_paths.rs`, or `cmd.rs`.

## Critical files

- `cli/src/doctor.rs` — main edit. Extract `DiskState`, `classify_disk_state`, `summarize_declared_disks`; rewrite `check_declared_disks` as a thin wrapper. Region: lines ~176-240 today.
- `README.md:285-290` — one-line update to the `braid doctor` summary line so it reads something like `check config, pool health, profile consistency, LUKS headers`. The comment annotation is the user-visible spec.
- `tests/cli/braid-doctor.py` — append the new corruption subtest at the end.
- `cli/src/cmd.rs:68-70` — existing `CmdRequest::CryptsetupIsLuks` / `CryptsetupLuksDumpText` variants. **Read-only reference.**
- `cli/src/membership.rs:30-73` — `PoolMembership` / `DiskMember` types. **Read-only reference.**

## Verification

### Unit tests on `summarize_declared_disks` (pure, in `cli/src/doctor.rs` test module)

Because `summarize_declared_disks` is pure and takes a `&[(String, String, DiskState)]`, tests build classifications by hand — no `MockRunner`, no block-device fabrication, no temp `StatePaths`, no backup-file stubs. Each test gets the block-comment header per AGENTS.md test conventions.

1. **`summarize_ok_when_all_headers_intact`** — all disks `LuksHeaderOk`. Expect `CheckStatus::Ok` and the existing "all N declared disk(s) present" message. Confirms the happy path regression.
2. **`summarize_warn_luks_header_unreadable`** — one disk `LuksHeaderUnreadable`. Assert `Warn`; the message contains the disk name, the phrase "header unreadable", "off-system" (or equivalent), and the literal `luksHeaderRestore` token. Critically, the message must **not** contain `/var/lib/braid/luks-headers/` or `.luksheader` — the test pins the cross-command consistency invariant in CI.
3. **`summarize_warn_luks_header_damaged`** — one disk `LuksHeaderDamaged`. Assert `Warn`, that the message contains `cryptsetup repair --type luks2`, and that it explicitly tells the user to make a safe backup first. Also assert the message does not reference `/var/lib/braid/luks-headers/` or `.luksheader`.
4. **`summarize_warn_probe_failed_does_not_suggest_repair`** — one disk `ProbeFailed("simulated runner error")`. Assert `Warn`, that the message surfaces the error string, and — critically — that the message does **not** contain `cryptsetup repair` or `luksHeaderRestore`. This is the executable form of the earlier Medium finding: probe-execution failure must never masquerade as header corruption.
5. **`summarize_preserves_missing_and_not_block_messages`** — one `Missing`, one `NotBlock`, rest `LuksHeaderOk`. Assert `Warn` and that the existing "not found" / "not a block device" phrasing still appears. Protects against accidental regression of the current messages during the refactor.
6. **`summarize_mixed_states_reports_all`** — multiple disks across multiple failure categories. Assert that every failing disk's name appears in the message.

Run with `just test-rust`.

### VM test in `tests/cli/braid-doctor.py` (in scope)

Per Principle 8, the detection half of the change needs end-to-end coverage against real cryptsetup. Unit tests on `summarize_declared_disks` cannot prove that `classify_disk_state` returns the right `DiskState` for a real damaged header — only a VM test can.

Add one new subtest at the end of `tests/cli/braid-doctor.py`, after the existing "mixed profiles" subtests and before `machine.shutdown()`:

- **`Corrupted LUKS header — declared_disks warns`**
  1. The pool is already up at this point in the file (disks were added earlier at `tests/cli/braid-doctor.py:122-127`).
  2. Wipe the LUKS header on disk1's underlying block device with an aligned, direct-I/O write:
     `dd if=/dev/zero of=/dev/disk/by-id/virtio-disk1 bs=1M count=16 conv=notrunc oflag=direct status=none`,
     followed by `sync && echo 3 > /proc/sys/vm/drop_caches`.
     **`oflag=direct` is required** — a buffered small write (e.g. `bs=1 count=16`) lands in the page cache and is not visible to cryptsetup's subsequent read, so the test will falsely report the disk as healthy. 16 MiB is chosen to safely cover the entire LUKS2 header + binary keyslot area. `drop_caches` invalidates any read cache that may still be holding a stale header. The running pool keeps working through all of this — the kernel's dm-crypt mapping is held in memory, so the test does not need to tear the pool down. Only fresh `cryptsetup` reads of the on-disk header are affected, which is exactly what `classify_disk_state` does.
  2a. Sanity-check: before asserting on doctor output, run `cryptsetup isLuks /dev/disk/by-id/virtio-disk1` directly and assert its exit code is non-zero. If this sanity check ever fails, it points unambiguously at the dd recipe rather than at the doctor logic under test — this is defense against the failure mode described above.
  3. Run `braid doctor --json` and parse the report.
  4. Assert `checks["declared_disks"]["status"] == "warn"`.
  5. Assert the message mentions `disk1` by name.
  6. Assert the message contains the phrase `header unreadable` and the literal `luksHeaderRestore` token.
  7. Assert the message does **not** contain `cryptsetup repair` (proves we classified as `LuksHeaderUnreadable`, not `LuksHeaderDamaged`).
  8. Assert the message does **not** contain `/var/lib/braid/luks-headers/` or `.luksheader`. This pins the product invariant at the VM-test level too: doctor never points users at local header copies.

What we deliberately do not cover at the VM level:

- **`LuksHeaderDamaged`** (isLuks ok, luksDump fail) requires surgical corruption of LUKS2 metadata that leaves the magic bytes intact. The mechanics are brittle and offer little marginal coverage over the unit test on the same code path. Unit-only is sufficient.
- **`Missing`**, **`NotBlock`**, and **`ProbeFailed`** are message-rendering decisions fully covered by unit tests on `summarize_declared_disks`.

Run with `just test-vm braid-doctor`.

### Manual smoke test (developer, not required for merge)

Same recipe as the VM test, run interactively against any braid VM you happen to have up.

## Out of scope / follow-ups

- Improving the unlock-time error message in `luks.rs:230+` so `cryptsetup open` failures emit the same generic off-system-backup and `cryptsetup repair` guidance that `summarize_declared_disks` now produces in doctor. Natural continuation: extends the cross-command consistency invariant to the remaining touchpoint (unlock) where users actually hit header corruption in the wild. Same motivation, different code path, separate PR.
