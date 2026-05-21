# Surface pending-op.json as a status advisory

## Context

Principle 3 in `docs/principles.md:22` says that when `/var/lib/braid/pending-op.json` exists, braid is in recovery mode and only `status`, `recover`, and `lock` are permitted. `manual/guides/recovery-scenarios.md:112-116, 147` directs operators to `braid status` to triage and to confirm the journal is stale before clearing it.

Today `cli/src/status.rs:383-479` (`build_status`) never reads `paths.pending_op_json()`. If the pool is mounted with a stranded journal (e.g. recover succeeded on topology but failed on follow-up balance; or an operator manually mounted instead of running `recover`), `braid status` renders a normal-looking report with no hint that `braid recover` is owed. `cli/src/preflight.rs:41-57` (`check_no_pending_operation`) is the gate other commands use, but status -- as one of the three recovery-mode-permitted commands -- has no equivalent surface.

The `advisories` channel is already the established home for "side state the operator should know about" (foreign filesystem at mount point, pending LUKS header backups). Commit `759b299 fix(status): surface foreign fstype advisory` (2026-05-18) added the most recent analog and is the template to follow.

Intended outcome: `braid status` (human and `--json`) prints a single `warning:` line whenever pending-op.json exists, directing the operator to `braid recover`. Behavior is identical regardless of mount state: mounted, not-mounted, or NotBtrfs.

## Approach

Add a pure helper in `cli/src/journal.rs` next to `load_journal`, and refactor `build_status`'s advisory assembly so it is built in explicit severity order at every return path. The helper handles three cases: absent (no advisory), present (advisory with `started_at`), and corrupt (canonical pending-op remediation phrase, not "run braid recover").

**Severity order (highest first):** foreign-fstype obstruction > pending recovery journal > LUKS header backups. The current code emits header-backups first and foreign-fstype last, which is already inverted; this fix straightens it. All three `build_status` return paths (mounted, not-mounted, NotBtrfs) flow through a single private helper `assemble_advisories(paths, foreign_fstype: Option<String>) -> Vec<String>` so the order is enforced in exactly one place.

The TUI (`cli/src/tui/probe.rs`) also has no pending-op surface and would benefit from the same helper, but that is a separate gap and not part of this fix.

The richer surface from the original `cheerful-prancing-hearth.md` plan ("display journal contents (op type, started_at, pre vs target membership diff)") is intentionally **out of scope**. The minimal advisory is sufficient for the safety goal -- visibility on triage. A diff render is a JSON-schema-changing feature, not a fix.

## Critical files

- `cli/src/journal.rs:242-253` -- add `pub fn pending_op_advisories(paths: &StatePaths) -> Vec<String>` immediately after `load_journal`. Three branches: `Ok(Some(j))` -> one advisory using `j.started_at` and pointing to `braid recover`; `Ok(None)` -> empty; `Err(e)` -> one advisory that does **not** route the operator to `braid recover`. For `JournalError::Parse`, the variant's pinned Display already carries the canonical phrase `Remove /var/lib/braid/pending-op.json after manual reconciliation (see docs/luks-unlock.md) and re-run.` so the helper can emit `e.to_string()` verbatim. For other variants (e.g. `JournalError::Io { source: PermissionDenied, .. }`), append the canonical phrase manually so the operator never lands in a recover/status loop.
- `cli/src/status.rs:383-479` -- introduce a private `assemble_advisories(paths: &StatePaths, foreign_fstype: Option<String>) -> Vec<String>` helper that builds the vec in severity order. Replace the current `let mut advisories = luks::header_backup_advisories(paths); ...; advisories.push(e.to_string());` flow with two `assemble_advisories(...)` call sites: the `NotBtrfs` branch passes `Some(e.to_string())`; every other path passes `None`. No other change to `not_mounted_status` is needed; both return paths already render `advisories` via `cli/src/status.rs:983-985` (`warning: <text>\n`) and serialize it under `advisories` in JSON.
- `manual/commands/status.md:144-179` -- add a new "Pending recovery journal" subsection under "Advisories", positioned between "Foreign filesystem at the mount point" and "Pending LUKS header backups". Quote both the journal-present and the journal-unreadable lines, and point readers to `docs/luks-unlock.md#unparseable-state-file-reconciliation` (the section at `docs/luks-unlock.md:143` that pins the canonical safe-to-remove conditions) for the manual-reconciliation workflow.

## Existing patterns to reuse

- **Helper shape**: `cli/src/luks.rs:1119` `pub fn header_backup_advisories(paths: &StatePaths) -> Vec<String>` is the public-wrapper analog. The luks variant has a private `_in(dir: &Path)` inner because it scans a directory; the journal variant does not need an inner helper because `load_journal(paths)` already does the file I/O. Keep the new helper a single function -- do not introduce an inner _in/_at split.
- **Advisory render**: `cli/src/status.rs:983-985` iterates `report.advisories` and prefixes each with `warning: `. No changes needed.
- **Pure-helper test grain**: `cli/src/journal.rs:382` `fn roundtrip_add_multi_target_uuid_sorted` and `cli/src/journal.rs:437` `fn roundtrip_remove_variant` use `tempfile::TempDir::new()` + `StatePaths::custom()` with no command runner. Same grain for the new helper tests.
- **Integration-test analog**: `cli/src/status.rs:2786` `fn build_status_not_btrfs_surfaces_fstype_advisory` is the exact template for the new integration test -- mock runner returning a sensible pool, write a stub `pending-op.json` into the StatePaths root, assert `built.report.advisories` contains the expected line.
- **Preflight wording source**: `cli/src/preflight.rs:46-51` -- the new advisory mirrors its opening phrase ("interrupted operation detected (pending-op.json exists, started {}") for cross-command consistency.

## Advisory wording

Confirmed with user. Rendered as `warning: <text>\n`:

- **Journal present**: `interrupted operation detected (pending-op.json exists, started <iso8601>) -- run 'braid recover' to reconcile`
- **Journal unreadable (`JournalError::Parse`)**: passes through the variant's pinned Display verbatim -- `failed to parse pending-op.json: <detail>. Remove /var/lib/braid/pending-op.json after manual reconciliation (see docs/luks-unlock.md) and re-run.`
- **Journal unreadable (other `JournalError` variants, e.g. `Io`)**: `<Display text>. Remove /var/lib/braid/pending-op.json after manual reconciliation (see docs/luks-unlock.md) and re-run.`

The unreadable advisories deliberately do **not** point at `braid recover`. `cli/src/recover.rs:1242-1252` shows recover's first step is `journal::load_journal`, so any state that makes the helper return `Err` also makes recover return the same error -- routing the operator there would create a recover/status loop and contradict the canonical phrase pinned in `cli/src/journal.rs:204-208`, `docs/luks-unlock.md:178-180`, and `manual/guides/recovery-scenarios.md:138-140`.

Journal-present wording style matches `cli/src/preflight.rs:46-51`; unreadable wording is the canonical phrase pinned in `JournalError::Parse`'s Display.

## Tests

**Pure-helper tests** -- add to `cli/src/journal.rs` `#[cfg(test)] mod tests`:

1. `fn pending_op_advisories_empty_when_absent` -- tempdir StatePaths with no `pending-op.json`. Assert `pending_op_advisories(&paths).is_empty()`.
2. `fn pending_op_advisories_present_includes_started_at` -- write a valid journal via `write_journal` (reuse existing helpers in the test module). Assert the returned Vec has exactly one entry, contains `"interrupted operation detected"`, contains the literal `started_at` string from the written journal, and contains `"braid recover"`.
3. `fn pending_op_advisories_unparseable_uses_canonical_remediation` -- write `"not json"` directly to `paths.pending_op_json()` (triggers `JournalError::Parse`). Assert exactly one entry containing the canonical phrase `"Remove /var/lib/braid/pending-op.json after manual reconciliation (see docs/luks-unlock.md) and re-run."`. Also assert it does **not** contain the substring `"braid recover"`, locking the no-loop invariant. (Mirrors `cli/src/preflight.rs:1228-1235` `pending_op_refuses_on_corrupt_journal`.)
4. `fn pending_op_advisories_io_error_uses_canonical_remediation` -- create a **directory** at `paths.pending_op_json()` via `std::fs::create_dir`, so `load_journal`'s `read_to_string` returns `EISDIR` and the helper takes the `JournalError::Io` branch (the only other variant `load_journal` can produce besides `Parse`). Assert exactly one entry containing the canonical phrase `"Remove /var/lib/braid/pending-op.json after manual reconciliation (see docs/luks-unlock.md) and re-run."` and **not** containing `"braid recover"`. Pins the Io-branch manual-append code path that test 3 cannot reach.

Each test preamble follows the project convention (`Intent` / `Why it exists` / `Scenario`).

**Integration tests** -- add to `cli/src/status.rs` `#[cfg(test)] mod tests`, near `build_status_not_btrfs_surfaces_fstype_advisory` (line 2786). One test per `build_status` return path so the "regardless of mount state" claim is mechanically pinned:

5. `fn build_status_surfaces_pending_op_advisory_when_mounted` -- mounted-btrfs fixture (`status_fs_mounted` / `status_fs_three_disk` per the existing healthy-pool tests) + stub `pending-op.json` written to the StatePaths root. Assert `built.report.advisories` contains an entry whose text starts with `"interrupted operation detected (pending-op.json exists, started "`.
6. `fn build_status_surfaces_pending_op_advisory_when_not_mounted` -- `status_fs_not_mounted` fixture + stub `pending-op.json`. Assert `built.report.status == StatusCode::NotMounted` and the same advisory text is present in `built.report.advisories`. Pins coverage on the `not_mounted_status` return path.
7. `fn build_status_orders_foreign_fstype_before_pending_op_advisory` -- `status_fs_ext4` fixture + stub `pending-op.json`. Assert both the foreign-fstype advisory and the pending-op advisory are present, and that the foreign-fstype text appears at index 0 with the pending-op text at index 1. Pins the `NotBtrfs` return path **and** the severity-order invariant in one assertion.

The existing `build_status_not_btrfs_surfaces_fstype_advisory` (line 2786) continues to pass unchanged because it writes no `pending-op.json` and its `assert_eq!` against the exact advisories vec stays valid.

No new VM test is needed; this is a pure-Rust read-only render change and the existing Rust test surface is the right grain. Existing VM tests that exercise mid-recovery flows (`tests/module/recover_*.py` etc.) continue to pass because they do not assert on the absence of the new advisory text.

## Documentation

`manual/commands/status.md` -- add a third subsection under "### Advisories" (currently at line 144). Place it **between** "Foreign filesystem at the mount point" and "Pending LUKS header backups" (chronological severity: recovery-mode is more urgent than a backup-file reminder, less urgent than a foreign-mount obstruction).

New subsection content (literal):

```markdown
**Pending recovery journal.** When `/var/lib/braid/pending-op.json`
exists, an interrupted `add` / `remove` / `remove-missing` / `replace`
is owed. `braid status` prints the advisory whether or not the pool is
mounted:

\`\`\`
warning: interrupted operation detected (pending-op.json exists, started 2026-05-20T10:30:00Z) -- run 'braid recover' to reconcile
\`\`\`

Run `sudo braid recover` to reconcile from live pool state; do not
remove `pending-op.json` by hand except under the conditions documented
in [Pending-op file corruption](../guides/recovery-scenarios.md#pending-op-file-corruption).
If the journal is unreadable, the advisory carries the canonical
manual-reconciliation phrase instead -- because `braid recover` cannot
load an unparseable journal either:

\`\`\`
warning: failed to parse pending-op.json: <detail>. Remove /var/lib/braid/pending-op.json after manual reconciliation (see docs/luks-unlock.md) and re-run.
\`\`\`

See [Unparseable state-file reconciliation](../../docs/luks-unlock.md#unparseable-state-file-reconciliation)
for the safe-to-remove conditions.
```

No change needed to the "JSON output" subsection -- the new advisory lands in `advisories` automatically and the existing description ("array of human-readable advisory strings ... see the Advisories section above for what currently produces them") still applies.

## Verification

1. `just test-rust` -- runs all Rust unit tests, including the seven new tests above. Exit 0.
2. `just test-vm cmd-status` (or the closest existing status VM test) -- existing test passes unchanged (no behavioral regression for the journal-absent path).
3. Manual: in a dev VM, `sudo touch /var/lib/braid/pending-op.json; sudo braid status` and confirm:
   - Non-JSON output prints the canonical `failed to parse pending-op.json: ... Remove ... after manual reconciliation (see docs/luks-unlock.md) and re-run.` line above `Pool:` (empty file is not valid JSON).
   - The line does **not** contain `braid recover`.
   - `sudo braid status --json | jq .advisories` lists exactly one entry containing `"after manual reconciliation"`.
   - `sudo rm /var/lib/braid/pending-op.json && sudo braid status` -- advisory is gone.
4. (Optional, gives the present-but-valid path a real journal): trigger a real interrupted mutation -- e.g. start `braid add` against a fresh disk and `kill -9` the process between journal write and pool commit. Then `sudo braid status` should print the "interrupted operation detected" advisory with a real `started_at` and the recommendation to run `braid recover`.

## Out of scope

- **TUI**: `cli/src/tui/probe.rs` also has no pending-op awareness. Defer to a follow-up so this fix stays minimal; the new `journal::pending_op_advisories` is the helper that follow-up will reuse.
- **Richer journal surface in `--json`**: a dedicated `pending_operation` JSON field with op kind and pre/target diffs is what `cheerful-prancing-hearth.md` originally envisioned. That is a feature on top of this fix, not part of it.
- **`braid lock`**: also recovery-mode-permitted, but it is an unmount operation and does not render an operator-facing status report. No change.
