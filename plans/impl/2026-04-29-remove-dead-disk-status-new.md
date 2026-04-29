# Remove dead `DiskStatus::New` variant

## Context

`DiskStatus::New` (cli/src/status.rs:152) is defined and rendered, but no
production code constructs it. The two builders that populate disk status
(`build_compact_drives`, `build_disk_reports`) only ever produce `Present`,
`Missing`, `Unknown`, `LuksHeaderUnreadable`, or `LuksHeaderDamaged`. The
variant is exercised solely by tests that hand-build `CompactDrive`/`HumanDisk`
literals to drive an unreachable formatter path, plus one tautological
"healthy output never contains `new`" guard. Removing it shrinks the status
schema by one unused value, deletes a formatter branch that cannot fire, and
removes ~80 lines of test scaffolding for behavior that has no real-world
trigger.

This is safe: there is no positive consumer of the JSON `"new"` value (Python
VM tests, `docs/`, `modules/`, and `README.md` were searched). The Python
test's `assert "new" not in output.lower()` at `tests/cli/braid-status-rust.py:78`
is a defensive negative check that becomes vacuous after removal. The
user-facing manual at `manual/commands/status.md:141` documents `NEW` as a
disk-state row in the detail-view table and must be updated to match the new
schema. (`manual/book/` is CI-generated and not tracked.)

## Changes

All edits are in **`cli/src/status.rs`** unless noted.

1. **Drop the enum variant** at line 152 — remove the trailing `New,` line from
   the `DiskStatus` enum (lines 146-153).

2. **Drop the `Display` arm** at line 163 — remove `Self::New => f.write_str("new"),`
   from the `Display for DiskStatus` impl (lines 155-166). The other five arms
   stay — `Display` is still used at line 1044 (compact listing) and line 1109
   (verbose `Present` arm).

3. **Drop the formatter match arm** in `format_status_human` at lines 1091-1093:

   ```rust
   DiskStatus::New => {
       out.push_str(&format!("  {:<18}NEW\n", d.name));
   }
   ```

   The surrounding `match d.status { ... }` (lines 1087-1111) becomes
   exhaustive over the remaining five variants. No other match expression in
   the file needs updating — the other DiskStatus matches at lines 1142-1158
   already use `==` comparisons against specific variants, never `New`.

4. **Delete three tests** in the `#[cfg(test)] mod tests` block:
   - `status_compact_new_disk` (lines 3489-3522) — synthetic `CompactDrive`
     with `DiskStatus::New`, asserts output contains `"new"`.
   - `status_verbose_new_disk` (lines 3545-3585) — synthetic `HumanDisk` with
     `DiskStatus::New`, asserts output contains `"NEW"`.
   - `status_human_healthy_no_new` (lines 3632-3678) — only `Present` drives,
     asserts `!human.contains("new")`. After removal the formatter cannot
     emit `"new"` from any code path, so this assertion is a structural
     tautology.

   Also remove the section-header comment block at lines 3485-3487
   (`// Compact drive tests`) only if it leaves an orphan header; the
   `status_compact_missing_disk` test at line 3525 still belongs under that
   header, so leave the header in place. Likewise the headers at lines
   3541-3543 (`// Verbose new/unknown tests`) and 3628-3630 (`// Healthy
   tests assert no "new"`) should be retitled or removed depending on what
   remains. Concretely:
   - Lines 3541-3543: rename "Verbose new/unknown tests" -> "Verbose unknown
     tests" (the `status_verbose_unknown_disk` test at 3587 stays).
   - Lines 3628-3630: remove the entire header block since
     `status_human_healthy_no_new` is the only test in that section.

5. **Drop the now-vacuous Python assertion** at
   `tests/cli/braid-status-rust.py:78`:
   ```python
   assert "new" not in output.lower(), f"Unexpected 'new':\n{output}"
   ```
   The companion `assert "missing" not in output.lower()` on line 77 stays --
   `Missing` is a real variant. Removing line 78 keeps the test focused on
   live invariants.

6. **Drop the manual table row** at `manual/commands/status.md:141`:
   ```
   | **NEW** | Disk detected but not yet part of the pool |
   ```
   Remove the entire line. The surrounding table at lines 134-141 stays
   intact with the remaining five rows (`present`, `MISSING`, `LUKS HEADER
   UNREADABLE`, `LUKS HEADER DAMAGED`, `UNKNOWN`).

## Files modified

- `cli/src/status.rs` — variant, Display arm, formatter arm, three tests,
  two header-comment edits.
- `tests/cli/braid-status-rust.py` — drop one assertion.
- `manual/commands/status.md` — drop the `NEW` row from the disk-state
  table.

## Verification

1. **Compile + Rust unit tests**: `just test-rust`. Expected: all tests pass;
   the three deleted tests no longer run; no other test references `New` (the
   Explore pass confirmed this).

2. **Exhaustiveness check**: `cargo check -p braid-cli` (covered by
   `just test-rust`). The `match d.status` at status.rs:1087 must compile
   without a wildcard, proving the five remaining arms are exhaustive.

3. **JSON schema check**: run `braid status --json` against an existing VM
   test scenario and confirm no `"status": "new"` ever appears (it never did,
   but this confirms parity).

4. **VM lane**: `just test-vm braid-status-rust` -- exercises the modified
   Python assertion and the live formatter against a healthy 3-disk RAID1.
   Expected: `Healthy RAID1 summary` subtest passes with line 78 gone.

5. **Parser canary**: `just test-parsers` -- not strictly required (no parser
   touched), but cheap insurance that the status command still functions
   end-to-end in a VM.

No fixture refresh needed (no parser-critical tool versions involved).
