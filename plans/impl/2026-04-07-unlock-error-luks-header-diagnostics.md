# Plan: enrich unlock-time errors with LUKS header diagnostics

## Context

This is the natural follow-up to `plans/impl/2026-04-07-doctor-detect-luks-header-corruption.md`. That plan made `braid doctor` probe LUKS headers on declared disks and emit actionable remediation messages distinguishing three failure modes (unreadable, damaged-metadata, probe-execution-failure), with strict cross-command consistency: messages must never reference local `/var/lib/braid/luks-headers/*.luksheader` files, because those are transient artifacts the user is expected to export off-system and then delete — and `braid status` / the TUI already warn about persistent local copies.

Doctor is the *proactive* touchpoint, but most users don't run it on a schedule. In practice, the place where users actually hit header corruption is `braid unlock`: the pool boots, they ssh in, they try to open it, and something goes wrong. Today's error at that moment is either:

- `"wrong passphrase (verified against <disk>)"` — emitted when `luks::verify_passphrase` on the first disk in the unlock set returns `false`. `verify_passphrase` uses `cryptsetup --test-passphrase` and returns `Ok(true/false)` with no distinction between "wrong passphrase" and "header metadata damaged such that no keyslot matches." So a disk with damaged LUKS2 metadata gets blamed as a typo.

- `"failed to open disk '<name>': passphrase was verified against '<first>' but rejected here — <hint> (<stderr>). If the passphrase is correct, the single-passphrase invariant may be violated by external LUKS manipulation"` — emitted in the per-disk open loop at `cli/src/mount.rs:282-293` (passphrase) and `cli/src/mount.rs:246-257` (keyfile) when `ensure_luks_open` returns `LuksError::OpenFailed { exit_code: 2, .. }`. Exit 2 means EPERM, which covers both "wrong passphrase for this disk" AND "damaged keyslot metadata could not produce a valid key." The current message assumes the former.

Both of these are misdiagnoses — they tell the user to check their passphrase or suspect external manipulation when the real problem is header corruption that `cryptsetup repair` or an off-system backup restore could address.

The existing `plan_open_pool` probe step at `cli/src/mount.rs:88` already catches the "header fully unreadable" case via `cryptsetup luksUuid` failing → `ConfigDiskState::PresentNotLuks` → disk marked missing. That path is imperfect (the message reads "LUKS header damaged" instead of "unreadable") but it is not *wrong* enough to fix in this PR. Scope stays on the verify/open-loop misdiagnosis gap.

The fix: probe the affected disk's LUKS header immediately after a verify or open failure, and use the probe result to pick the right user-facing message. Doctor and unlock then share:

- The probe primitive (`probe_luks_header` + `LuksHeaderState` enum).
- The two remediation message strings (unreadable → off-system guidance, damaged → `cryptsetup repair --type luks2` with safe-backup warning).
- The negative invariants: neither command ever references `/var/lib/braid/luks-headers/` or `.luksheader`.

### Alignment with `docs/principles.md`

- **Principle 3 (safe-by-construction)** — `braid unlock` remains read-only from the LUKS-header perspective. The probe only runs *after* an open attempt has already failed; it uses the same read-only cryptsetup commands (`isLuks`, `luksDump`) that doctor already uses. No command is auto-invoked; the `cryptsetup repair` suggestion is always paired with an explicit "make a safe backup first" warning.
- **Principle 4 (single passphrase)** — the existing "single-passphrase invariant may be violated by external LUKS manipulation" message stays as the fallback when a disk's header *is* intact and the exit code is 2 on a subsequent disk. That's exactly the scenario the message was written for; we only divert away from it when the probe proves the header is damaged.
- **Principle 8 (test every design decision)** — both halves covered: unit tests for the enrichment decision logic, and a new VM subtest in `tests/cli/braid-unlock.py` that corrupts a disk's LUKS header, runs `braid unlock`, and asserts the new guidance appears.
- **Cross-command consistency** — `doctor`, `unlock`, `status`, and the TUI all tell the same story about LUKS header corruption: generic off-system backup guidance, no pointers at local `.luksheader` files, and `cryptsetup repair` only ever offered with a safe-backup caveat.

## Scope

Four files edited:

1. **`cli/src/luks.rs`** — add the shared `LuksHeaderState` enum, `probe_luks_header` function, and the two guidance-string helpers (`luks_header_unreadable_guidance`, `luks_header_damaged_guidance`). These are the new primitives that both doctor and unlock will call.
2. **`cli/src/doctor.rs`** — refactor `classify_disk_state` to delegate the LUKS probe to `luks::probe_luks_header`, and rewrite the `LuksHeaderUnreadable` / `LuksHeaderDamaged` match arms in `summarize_declared_disks` to call the shared guidance helpers instead of inline strings. No behavior change.
3. **`cli/src/mount.rs`** — enrich four failure paths inside `open_and_mount_pool`: both the passphrase and keyfile verify-step failures, plus both the passphrase and keyfile per-disk-open-loop failures. The classification logic is shared via a new `explain_open_failure` helper that handles all four `LuksHeaderState` branches:
   - `Unreadable` / `Damaged` → emit the corresponding shared guidance (overrides whatever cryptsetup said).
   - `Ok` → use the caller-supplied fallback unchanged (preserves the existing "wrong passphrase (verified against X)" and "single-passphrase invariant may be violated" messages for the intact-header cases they were designed for).
   - `ProbeFailed` → emit a dedicated "diagnosis could not be completed" message that includes both the original cryptsetup signal and the probe error. **Critically, `ProbeFailed` does NOT fall back to the passphrase/invariant wording** — that would reintroduce the same misdiagnosis this plan is meant to fix whenever the probe itself fails (e.g. cryptsetup missing from PATH).
4. **`tests/cli/braid-unlock.py`** — new subtest at the end of the file: corrupts disk2's LUKS header with the established `bs=1M oflag=direct` + `sync` + `drop_caches` recipe, runs `braid unlock` expecting failure, and asserts on the new guidance (including the negative invariant that the message does NOT contain `/var/lib/braid/luks-headers/` or `.luksheader`).

## Design

### 1. Shared primitives in `luks.rs`

Add near the existing cryptsetup exit-code helpers (`classify_cryptsetup_exit` at `cli/src/luks.rs:196`):

```rust
/// Outcome of probing a LUKS device's on-disk header. Used by both
/// `braid doctor` (for declared-disk health checks) and `braid unlock`
/// (for enriching open-failure errors with the real cause).
#[derive(Debug, Clone)]
pub(crate) enum LuksHeaderState {
    /// Both `isLuks` and `luksDump` succeeded; the header is intact.
    Ok,
    /// `isLuks` exited non-zero — the LUKS magic is gone or the header
    /// is otherwise unreadable. Severe.
    Unreadable,
    /// `isLuks` succeeded but `luksDump` exited non-zero — the magic
    /// is intact but LUKS2 metadata is damaged.
    Damaged,
    /// The cryptsetup command failed to execute (missing binary, IPC
    /// failure). NOT the same as cryptsetup finding corruption — callers
    /// must never emit repair/restore suggestions in this state.
    ProbeFailed(String),
}

/// Read-only LUKS header probe. Runs `cryptsetup isLuks` then
/// `cryptsetup luksDump` and classifies the result. Safe to call on a
/// device that is currently open via dm-crypt — the probe reads the
/// raw block device, not the mapper.
pub(crate) fn probe_luks_header<R: CommandRunner>(runner: &R, device: &str) -> LuksHeaderState {
    match runner.run(&CmdRequest::CryptsetupIsLuks { device: device.to_owned() }) {
        Err(e) => return LuksHeaderState::ProbeFailed(e.to_string()),
        Ok(raw) if raw.exit_status != 0 => return LuksHeaderState::Unreadable,
        Ok(_) => {}
    }
    match runner.run(&CmdRequest::CryptsetupLuksDumpText { device: device.to_owned() }) {
        Err(e) => LuksHeaderState::ProbeFailed(e.to_string()),
        Ok(raw) if raw.exit_status != 0 => LuksHeaderState::Damaged,
        Ok(_) => LuksHeaderState::Ok,
    }
}

/// Guidance text for an unreadable LUKS header. Deliberately generic —
/// never references local `/var/lib/braid/luks-headers/` files. `braid
/// status` and the TUI already warn when local header backups persist
/// on the same machine because the intended product workflow is to
/// export them off-system and remove the local copy; doctor and unlock
/// must not contradict that posture by instructing users to rely on
/// local copies as a safety net.
pub(crate) fn luks_header_unreadable_guidance() -> &'static str {
    "LUKS header unreadable. Restore from your off-system LUKS header \
    backup if you have one (cryptsetup luksHeaderRestore). Without an \
    off-system backup, recovery may be limited or impossible."
}

/// Guidance text for a damaged-metadata LUKS header. Always pairs the
/// `cryptsetup repair` suggestion with an explicit safe-backup warning.
pub(crate) fn luks_header_damaged_guidance(device: &str) -> String {
    format!(
        "LUKS header metadata damaged. To attempt repair manually: \
        cryptsetup repair --type luks2 {device} — make a safe backup of \
        the device header before running repair."
    )
}
```

These go in `luks.rs` (not a new module) because they are cryptsetup-specific and naturally group with the existing `classify_cryptsetup_exit` / `cryptsetup_open_hint` helpers. `pub(crate)` is the right visibility — only doctor and mount need them, both in the same crate.

### 2. Refactor `doctor.rs` to use the shared helpers

At `cli/src/doctor.rs`, the existing private `probe_luks_header`-like logic inside `classify_disk_state` and the inline message strings in `summarize_declared_disks` get replaced with calls into `luks`. This is a mechanical refactor with no behavior change:

- `classify_disk_state` keeps the filesystem gate (`Missing` / `NotBlock`) and then delegates to `luks::probe_luks_header`, mapping the returned `LuksHeaderState` onto the existing `DiskState` enum variants. The `DiskState` enum stays private to doctor because it also carries the fs-level variants (`Missing`, `NotBlock`) that unlock doesn't need.
- `summarize_declared_disks`'s match arms for `DiskState::LuksHeaderUnreadable` and `DiskState::LuksHeaderDamaged` replace their inline `format!` strings with `luks::luks_header_unreadable_guidance()` and `luks::luks_header_damaged_guidance(by_id)`. The per-disk `"{name} ({by_id}): "` prefix stays in doctor (it's doctor's aggregation context).

Existing doctor unit tests continue to pass because they assert on *substrings* (`"header unreadable"`, `"luksHeaderRestore"`, `"cryptsetup repair --type luks2"`, `"off-system"`, `"safe backup"`) that are present in the extracted helper strings verbatim. The negative assertions (`/var/lib/braid/luks-headers/` absent, `.luksheader` absent) also continue to hold.

### 3. Enrich unlock error paths in `mount.rs`

Two sites inside `open_and_mount_pool`. The enrichment logic is identical in shape for both; factoring it out is worth the small amount of shared code.

New private helper at the top of `mount.rs` (or just above `open_and_mount_pool`):

```rust
/// Classify an unlock-time failure against the LUKS header state of the
/// affected disk, producing the best user-facing error.
///
/// The four match arms each represent a distinct user story:
///
/// - `Unreadable` → corruption is confirmed severe; emit the off-system
///   backup guidance regardless of what cryptsetup originally said.
/// - `Damaged` → corruption is confirmed at the metadata level; emit
///   the `cryptsetup repair` guidance with a safe-backup warning.
/// - `Ok` → the header is intact, so the failure really is about the
///   passphrase / invariant / device state; use `ok_fallback` unchanged.
/// - `ProbeFailed` → we genuinely do not know whether the header is
///   sound, so we must NOT confidently blame the passphrase OR
///   corruption. Emit a dedicated "diagnosis incomplete" message that
///   surfaces both the original cryptsetup signal (`original_summary`)
///   and the probe error. This is load-bearing: if a failing cryptsetup
///   binary causes probe failures, the old design would still emit the
///   existing `"wrong passphrase"` / `"single-passphrase invariant"`
///   text, which is precisely the misdiagnosis this plan is meant to
///   stop.
fn explain_open_failure(
    disk_name: &str,
    device: &str,
    header_state: LuksHeaderState,
    original_summary: &str,
    ok_fallback: MountError,
) -> MountError {
    match header_state {
        LuksHeaderState::Unreadable => MountError::Failed(format!(
            "failed to unlock disk '{disk_name}' ({device}): {}",
            luks::luks_header_unreadable_guidance()
        )),
        LuksHeaderState::Damaged => MountError::Failed(format!(
            "failed to unlock disk '{disk_name}' ({device}): {}",
            luks::luks_header_damaged_guidance(device)
        )),
        LuksHeaderState::Ok => ok_fallback,
        LuksHeaderState::ProbeFailed(probe_err) => MountError::Failed(format!(
            "failed to unlock disk '{disk_name}' ({device}): {original_summary}. \
             LUKS header diagnosis could not be completed: {probe_err}. \
             Cannot distinguish a passphrase problem from LUKS header damage \
             — rerun with cryptsetup available on PATH, or inspect the disk \
             manually."
        )),
    }
}
```

The caller pre-builds both the one-line `original_summary` (used in the `ProbeFailed` branch so the user still sees what cryptsetup said) and the full `ok_fallback` (used when the header probe confirms the header is intact). Pre-building both is slightly redundant in the Unreadable/Damaged branches — the values get discarded — but the cost is negligible and the API is dead simple.

**Site A (verify-step failure).** Replace:

```rust
let ok = luks::verify_passphrase(runner, &first_by_id.0, &passphrase)?;
if !ok {
    return Err(MountError::Failed(format!(
        "wrong passphrase (verified against {})",
        first_name
    )));
}
```

with:

```rust
let ok = luks::verify_passphrase(runner, &first_by_id.0, &passphrase)?;
if !ok {
    let original_summary = format!("passphrase rejected on '{first_name}'");
    let ok_fallback = MountError::Failed(format!(
        "wrong passphrase (verified against {first_name})"
    ));
    let header_state = luks::probe_luks_header(runner, &first_by_id.0);
    return Err(explain_open_failure(
        first_name, &first_by_id.0, header_state, &original_summary, ok_fallback,
    ));
}
```

Identical-shape change at the `verify_key_file` site a few lines earlier (wording adjusted for keyfile — e.g. `"keyfile rejected on '{first_name}'"`).

**Site B (per-disk open loop).** Inside the existing `.map_err` closure for `ensure_luks_open`, wrap the result with a header probe before committing to either the invariant-violation message or the passthrough. Conceptually:

```rust
for (name, by_id) in &plan.to_unlock {
    if let Err(e) = luks::ensure_luks_open(runner, fs, name, by_id, &passphrase) {
        let header_state = luks::probe_luks_header(runner, &by_id.0);
        let (original_summary, ok_fallback) = match &e {
            LuksError::OpenFailed { exit_code: 2, hint, stderr, .. } => (
                format!("cryptsetup open rejected verified passphrase on '{name}' — {hint} ({stderr})"),
                MountError::Failed(format!(
                    "failed to open disk '{}': passphrase was verified against '{}' but \
                     rejected here — {} ({}). If the passphrase is correct, the \
                     single-passphrase invariant may be violated by external LUKS \
                     manipulation",
                    name, first_name, hint, stderr
                )),
            ),
            _ => (
                format!("cryptsetup open failed on '{name}': {e}"),
                MountError::Luks(e),
            ),
        };
        return Err(explain_open_failure(
            name, &by_id.0, header_state, &original_summary, ok_fallback,
        ));
    }
    eprintln!("{}  disk: {:<10}unlocked", tag("ok"), name);
}
```

Note: `e` is moved into `ok_fallback` via `MountError::Luks(e)` in the `_` arm, so `original_summary` has to be built from `e` *before* the move. In the real code, extract the `Display` representation first into a `String`, then construct both tuple members. Identical-shape change at the keyfile site.

`explain_open_failure` is called *unconditionally* after any failure, not just exit-code-2 — a header damaged enough to cause exit 1 (generic failure) also needs the right guidance.

### Things deliberately NOT done

- No changes to `plan_open_pool`'s existing `PresentNotLuks` handling at `cli/src/mount.rs:88`. The `"LUKS header damaged"` message there is imperfect terminology but not an error of substance; changing it would touch Test 7 in `braid-unlock.py` which asserts on that exact string. Separate concern, separate PR.
- No changes to `verify_passphrase` / `verify_key_file` signatures. They still return `Result<bool, LuksError>`; the enrichment lives entirely in `mount.rs`.
- No new `CmdRequest` variants.
- No shared guidance module — both helpers live in `luks.rs` because cryptsetup knowledge is already concentrated there.
- No reference to `/var/lib/braid/luks-headers/` or `.luksheader` anywhere in any new code or test.

## Critical files

- `cli/src/luks.rs` — add `LuksHeaderState`, `probe_luks_header`, `luks_header_unreadable_guidance`, `luks_header_damaged_guidance` near `cli/src/luks.rs:196`. Existing `ensure_luks_open` / `ensure_luks_open_with_key_file` / `verify_passphrase` / `verify_key_file` untouched.
- `cli/src/doctor.rs` — edit `classify_disk_state` (currently around lines 216-240) and the two match arms inside `summarize_declared_disks` (currently around lines 260-280). `DiskState` enum untouched.
- `cli/src/mount.rs` — add `explain_open_failure` helper, edit the verify-step and per-disk-open-loop failure paths in `open_and_mount_pool` (currently around lines 232-300).
- `tests/cli/braid-unlock.py` — append a new subtest after Test 8 (or before `machine.shutdown()`). See Verification.
- `cli/src/cmd.rs` — **read-only reference**: `CmdRequest::CryptsetupIsLuks` and `CryptsetupLuksDumpText` variants already exist, no changes.
- `plans/impl/2026-04-07-doctor-detect-luks-header-corruption.md` — **read-only reference** for the cross-command invariant and the `dd` recipe that actually works in the VM (`bs=1M count=16 conv=notrunc oflag=direct` + `sync; echo 3 > /proc/sys/vm/drop_caches`).

## Verification

### Unit tests on `luks.rs::probe_luks_header` (new, in the existing `cli/src/luks.rs` test module)

Use `MockRunner` to seed outputs for `CryptsetupIsLuks` and `CryptsetupLuksDumpText`. Each test gets the standard block comment header.

1. **`probe_luks_header_ok`** — mock both probes with exit 0. Assert `LuksHeaderState::Ok`.
2. **`probe_luks_header_unreadable_when_is_luks_fails`** — mock `isLuks` exit 1 with realistic stderr ("Device /dev/foo is not a valid LUKS device."). Assert `LuksHeaderState::Unreadable`. `luksDump` must not be called (mock has no output for it).
3. **`probe_luks_header_damaged_when_dump_fails`** — mock `isLuks` exit 0, `luksDump` exit 1. Assert `LuksHeaderState::Damaged`.
4. **`probe_luks_header_probe_failed_on_runner_error`** — use `MockRunner::default()` (no outputs seeded) which returns `Err(CmdError::MissingMock)`. Assert `LuksHeaderState::ProbeFailed(_)`.

### Unit tests on the guidance helpers (new, in `luks.rs` test module)

5. **`luks_header_unreadable_guidance_is_generic`** — call `luks_header_unreadable_guidance()`, assert it contains `"header unreadable"`, `"off-system"`, and `"luksHeaderRestore"`. Assert it does **not** contain `"/var/lib/braid/luks-headers/"` or `".luksheader"`. This is the single source of truth for the invariant; all downstream callers inherit the guarantee.
6. **`luks_header_damaged_guidance_interpolates_device_and_has_safe_backup_warning`** — call `luks_header_damaged_guidance("/dev/disk/by-id/wwn-0xDEAD")`. Assert it contains `"metadata damaged"`, `"cryptsetup repair --type luks2 /dev/disk/by-id/wwn-0xDEAD"`, and `"safe backup"`. Assert it does **not** contain `/var/lib/braid/luks-headers/` or `.luksheader`.

### Unit tests on the `explain_open_failure` pure helper (new, in `mount.rs` test module)

These target `explain_open_failure` directly (pure function, no I/O). Each test passes a hand-built `LuksHeaderState`, an `original_summary` string, and an `ok_fallback` `MountError`, and asserts on the resulting error message.

7. **`explain_open_failure_unreadable_overrides_fallback`** — `LuksHeaderState::Unreadable` + any fallback → message contains `"header unreadable"`, `"luksHeaderRestore"`, `"off-system"`. Negative: must not contain `/var/lib/braid/luks-headers/` or `.luksheader`. Must not contain the fallback's text (corruption overrides).
8. **`explain_open_failure_damaged_overrides_fallback`** — `LuksHeaderState::Damaged` + any fallback → message contains `"metadata damaged"`, `"cryptsetup repair --type luks2"`, `"safe backup"`, and the device path. Negative assertions for the local-backup strings.
9. **`explain_open_failure_ok_uses_fallback_verbatim`** — `LuksHeaderState::Ok` + a fallback containing `"wrong passphrase"` → result is exactly the fallback. Covers the intact-header case: existing messages must be preserved untouched.
10. **`explain_open_failure_ok_preserves_invariant_message`** — `LuksHeaderState::Ok` + a fallback containing `"single-passphrase invariant"` → result is exactly that fallback. Specific regression test for the invariant-violation case because it is the subtlest existing message.
11. **`explain_open_failure_probe_failed_emits_diagnosis_incomplete`** — `LuksHeaderState::ProbeFailed("simulated probe error")` + an `original_summary` of `"cryptsetup open rejected verified passphrase on 'disk2'"` + any fallback → message must contain `"diagnosis could not be completed"`, the `original_summary` text, AND the probe error. It must **not** contain the literal string `"wrong passphrase"` or `"single-passphrase invariant"` — these are the strings the old design would have leaked for probe-failed exit-2 cases, and this test is the executable form of the Ultraplan High finding. Also must not contain `/var/lib/braid/luks-headers/` or `.luksheader`.

### Unit tests on the enrichment wiring in `mount.rs::open_and_mount_pool` (new)

These target the full `open_and_mount_pool` function with a `MockRunner`, proving that all four enrichment call sites are wired correctly. Each test seeds the minimum cryptsetup outputs needed to reach the failure point and verifies the emitted error. The plan is to use the existing mount-test scaffolding (the module already has tests that mock `verify_passphrase` / `ensure_luks_open` paths; confirm during implementation and follow the established pattern).

Coverage matrix — one test per (call site × header state) combination that actually changes behavior:

12. **`unlock_passphrase_verify_fails_unreadable_header_emits_guidance`** — passphrase path, first-disk verify fails (mock `CryptsetupTestPassphrase` exit non-zero), mock `isLuks` exit non-zero. Assert error message contains `"header unreadable"` and `"luksHeaderRestore"`, and does NOT contain `"wrong passphrase"` or `/var/lib/braid/luks-headers/`.
13. **`unlock_passphrase_verify_fails_ok_header_preserves_wrong_passphrase`** — same as 12 but mock `isLuks`/`luksDump` both exit 0. Assert error contains `"wrong passphrase (verified against"`. This proves the intact-header fallback is still wired through the enrichment path.
14. **`unlock_keyfile_verify_fails_damaged_header_emits_repair_guidance`** — keyfile path, first-disk verify fails, mock `isLuks` exit 0, mock `luksDump` exit non-zero. Assert error contains `"cryptsetup repair --type luks2"` and `"safe backup"`.
15. **`unlock_passphrase_open_exit2_probe_failed_does_not_blame_invariant`** — passphrase path, verify succeeds on first disk, mock `CryptsetupLuksOpen` exit 2 on second disk, mock `isLuks` to return `CmdError` (use `MockRunner::default()` with no output seeded so the probe hits `MissingMock`). Assert the error contains `"diagnosis could not be completed"` and does NOT contain `"single-passphrase invariant"`. This is the integration-level form of test #11 — the Ultraplan High finding coverage at the actual call site.
16. **`unlock_keyfile_open_exit_nonzero_unreadable_header_emits_guidance`** — keyfile path, verify succeeds, mock `CryptsetupLuksOpenKeyFile` exit 1 on second disk, mock `isLuks` exit non-zero on that disk. Assert error contains `"header unreadable"`, `"luksHeaderRestore"`. Negative assertions for local-backup strings.

Tests 12-16 are belt-and-suspenders over the pure-helper tests 7-11 — they prove the call sites pass the right arguments into `explain_open_failure`, which the pure tests cannot catch on their own.

### Doctor regression (already in place)

The existing `doctor.rs` tests from the previous PR — `summarize_warn_luks_header_unreadable`, `summarize_warn_luks_header_damaged`, `summarize_warn_probe_failed_does_not_suggest_repair`, etc. — must still pass after the refactor that routes them through the shared helpers. No new assertions needed; these tests already pin the substrings.

Run all the above with `just test-rust`.

### VM test in `tests/cli/braid-unlock.py` — dropped mid-implementation

**This section documents an outcome the original plan got wrong.** The original plan called for a VM subtest that corrupts disk2's LUKS header and asserts `braid unlock` emits the new guidance. During implementation this proved to be unreachable via dd-based corruption:

- `plan_open_pool` runs a `cryptsetup luksUUID` probe on each declared disk *before* the per-disk open loop. `luksUUID` validates enough of the LUKS2 header structure that any dd-based corruption (from full wipe down to surgical JSON-area wipe at offsets 4K–32K) reliably trips it.
- When `luksUUID` fails, the disk is classified as `ConfigDiskState::PresentNotLuks` and the pool fails with the existing `"LUKS header damaged"` status line + degraded-refused error. That path is explicitly out of scope (the Context section notes this).
- The new enrichment path (verify-step + per-disk-open-loop failure → probe → Unreadable/Damaged/ProbeFailed) is only reached when a disk passes `luksUUID` but fails `cryptsetup open`, which requires surgical corruption of LUKS2 internal structures beyond what dd can reliably produce.

The VM test has been removed. A comment block in `tests/cli/braid-unlock.py` (before `machine.shutdown()`) documents why the path is unit-test-only, and the unit tests carry the full coverage:

- The 5 `explain_open_failure_*` tests cover the pure-helper match arms directly.
- The 5 `unlock_*` mount-integration tests drive `open_and_mount_pool` end-to-end with a `MockRunner` through all four enrichment call sites — including the critical `unlock_passphrase_open_exit2_probe_failed_does_not_blame_invariant` test that exercises the Ultraplan High finding at the actual call site.
- The `probe_luks_header` primitive is shared with `braid doctor` and is validated end-to-end against real cryptsetup by `tests/cli/braid-doctor.py` from the previous PR, which wipes a full LUKS header and asserts doctor's detection works.

The original planned subtest content is preserved below as historical context for what was attempted:

```python
# ORIGINAL PLANNED CONTENT — DO NOT IMPLEMENT:
# Intent: end-to-end coverage that braid unlock reports LUKS header
#   corruption with actionable, generic guidance — and never points users
#   at local /var/lib/braid/luks-headers/ files. The unit tests cover the
#   classification and message-rendering; this test covers the integration
#   between a real failed cryptsetup open and the enriched error output.
# Why it exists: previously, a disk with a damaged LUKS header was blamed
#   as "wrong passphrase" or "single-passphrase invariant violation" at
#   unlock time, leaving users chasing the wrong problem. The product
#   invariant is that unlock, doctor, status, and the TUI all tell the

```python
# Intent: end-to-end coverage that braid unlock reports LUKS header
#   corruption with actionable, generic guidance — and never points users
#   at local /var/lib/braid/luks-headers/ files. The unit tests cover the
#   classification and message-rendering; this test covers the integration
#   between a real failed cryptsetup open and the enriched error output.
# Why it exists: previously, a disk with a damaged LUKS header was blamed
#   as "wrong passphrase" or "single-passphrase invariant violation" at
#   unlock time, leaving users chasing the wrong problem. The product
#   invariant is that unlock, doctor, status, and the TUI all tell the
#   same story about LUKS header corruption.
# Scenario: a 3-disk pool is closed, disk2's LUKS header is wiped on the
#   underlying block device, and `braid unlock` fails. The first disk
#   (disk1) verifies successfully so we reach the per-disk open loop,
#   which fails on disk2; the probe then classifies disk2 as unreadable
#   and the user sees the right guidance.
with subtest("Corrupted LUKS header on subsequent disk — unlock emits generic guidance"):
    close_all()

    # Same recipe proven out in the doctor VM test. bs=1 count=16 does not
    # reliably corrupt the header (page cache hides the write from
    # cryptsetup's direct read); aligned direct I/O + drop_caches does.
    machine.succeed(
        "dd if=/dev/zero of=/dev/disk/by-id/virtio-disk2 bs=1M count=16 "
        "conv=notrunc oflag=direct status=none"
    )
    machine.succeed("sync && echo 3 > /proc/sys/vm/drop_caches")

    # Sanity: confirm cryptsetup itself now rejects disk2's header before
    # we blame the doctor-style enrichment for a false negative.
    is_luks_exit, _ = machine.execute(
        "cryptsetup isLuks /dev/disk/by-id/virtio-disk2"
    )
    assert is_luks_exit != 0, (
        "dd did not corrupt disk2's LUKS header: cryptsetup isLuks still succeeds"
    )

    # Redirect stderr so we can assert on the error text.
    cmd = unlock_cmd(passphrase) + " 2>&1"
    ret = machine.execute(cmd)
    assert ret[0] != 0, f"expected unlock to fail with corrupted disk2, got: {ret}"
    output = ret[1]
    print(f"unlock output with corrupted disk2:\n{output}")

    # The error must name disk2, describe it as unreadable, and point at
    # luksHeaderRestore with off-system backup language.
    assert "disk2" in output, f"missing disk2 in output: {output}"
    assert "header unreadable" in output, f"missing 'header unreadable': {output}"
    assert "luksHeaderRestore" in output, f"missing 'luksHeaderRestore': {output}"
    assert "off-system" in output, f"missing 'off-system': {output}"

    # The existing "wrong passphrase" / "invariant" messages must NOT
    # surface when the real cause is corruption.
    assert "wrong passphrase" not in output, (
        f"corruption case must not blame passphrase: {output}"
    )
    assert "invariant" not in output, (
        f"corruption case must not blame single-passphrase invariant: {output}"
    )

    # Cross-command consistency: never reference local header backup paths.
    assert "/var/lib/braid/luks-headers/" not in output, (
        f"unlock must not reference local backup directory: {output}"
    )
    assert ".luksheader" not in output, (
        f"unlock must not reference local .luksheader files: {output}"
    )
```

What we deliberately don't cover at the VM level (unit-test only):

- The verify-step corruption path (disk1 header damaged, caught in `verify_passphrase`). The enrichment logic is identical to the per-disk-open case; adding a second VM corruption scenario after the first would require leaving the VM in a state where only disk1 is corrupted, which is fragile to arrange.
- The `LuksHeaderDamaged` (metadata-only) case. Surgically damaging LUKS2 metadata while leaving the magic intact is brittle at the VM level and offers no marginal coverage over the unit test.
- The `ProbeFailed` case. Unit tests with `MockRunner::default()` prove the behavior; synthesizing a runner failure in a live VM would require tearing out cryptsetup from PATH.

Run with `just test-vm braid-unlock`.

### Manual smoke test (developer, not required for merge)

Boot any braid VM, set up a pool, close it, `dd if=/dev/zero of=<disk> bs=1M count=16 oflag=direct`, then `braid unlock` and confirm the new guidance appears and does not mention `/var/lib/braid/luks-headers/`.

## Out of scope / follow-ups

- Rename `plan_open_pool`'s `"LUKS header damaged"` status line to `"LUKS header unreadable"` for terminology consistency with the new cross-command language. Would touch `tests/cli/braid-unlock.py` Test 7's assertion string. Separate cosmetic PR.
- Richer degraded-refused error message that names specific disks with unreadable/damaged headers instead of the current generic "pool has missing devices" text. Worth doing but wider in scope; a separate PR.
