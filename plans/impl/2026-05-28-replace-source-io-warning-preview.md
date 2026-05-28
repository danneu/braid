# Move `braid replace` source-I/O-error warning into the plan note prelude

## Context

`braid replace` emits a "source device has I/O errors" warning only inside
`ReplacePlan::execute()` (`cli/src/replace.rs:784-799`), gated to the live
source. The warning is meant to inform an operator: *is the source healthy
enough to read from before I commit?* But it fires too late to do that:

- On `--dry-run` it never runs at all -- `cmd_replace` short-circuits at
  `replace.rs:1533-1536` (`plan.preview().print_colored(); return Ok(())`)
  before `execute()` is ever called.
- On a real run it is emitted after the confirmation prompt
  (`execute()` ~447-485), after `journal::write_journal` (~599), and after
  `cryptsetup luksFormat` (~619). By then the new disk is wiped and the
  journal is written -- the decision the warning implies is already past the
  point of no return.

`docs/commands/replace.md:116` documents it under "Safety checks" as
"Warns if the source device has I/O errors (informational, does not block)",
which an operator would reasonably expect to see during `--dry-run`.

**Outcome:** relocate the probe into `plan_replace` and push its result as a
`PreviewNote::Warn`, so it renders in `--dry-run` stdout and in the
pre-confirmation note prelude (notes render at `execute()` line 444 via
`emit_replace_notes_to_stderr`, before the prompt). This mirrors the existing
keyfile-asymmetry note (`replace.rs:1379`) and is exactly the pattern ADR 022
(`docs/design/decisions/022-dry-run-preview-model.md`) prescribes:
side-effect-free probes may run while building a preview, and `PreviewNote`s
are part of the structured stdout preview.

The underlying probe is read-only: `BtrfsDeviceStatsJson` builds
`btrfs --format json device stats <mount>` with no `-z`/reset flag
(`cli/src/cmd.rs:537-546`).

## Decisions (resolved)

- **Probe-failure handling: surface a `[warn]` note** (user-chosen). On runner
  error or parse error, push a probe-failure `PreviewNote::Warn` rather than
  swallowing it silently -- consistent with the keyfile-diagnostics block in
  the same function (`replace.rs:1363-1383`), which surfaces probe failures as
  notes. Fail-hard (`?`) is rejected: this is an informational, non-blocking
  diagnostic; aborting a real replace (or dry-run) because a stats read hiccuped
  would be a regression and violates the "does not block" contract.
- **Live-only (unchanged).** Keep the `ReplaceSource::Live` gate. A `Missing`
  source has no live device to stat, and the note's "btrfs replace will read
  from mirrors" wording is nonsensical for a device that isn't being read.
- **Note ordering: source-health note before the target-keyfile notes.**
  Insert right after `replace_source` resolution (~line 1302), yielding render
  order: preflight notes -> source-I/O note -> target-keyfile notes. This
  matches the operator's decision flow (can I read the source I'm evacuating?
  then: is the target set up right?) and groups the single source-concern note
  ahead of the target/pool-concern notes.

## Implementation

All edits are in `cli/src/replace.rs` unless noted.

### 1. Add two note-body helpers near `source_has_io_errors` (~line 1081)

`source_has_io_errors` (pure, `replace.rs:1081`) stays as-is. Add, co-located
with it (mirroring how `luks.rs:1096` co-locates `format_keyfile_asymmetry_warning`):

- `fn format_source_io_error_warning(devid: u64) -> String` -- the current
  message **with the literal `"Warning: "` prefix dropped** (the renderer adds
  `[warn]`; see `preview.rs:195`). Body otherwise verbatim:
  `source device (devid {devid}) has I/O errors. btrfs replace will read from
  mirrors where possible, but may fail if any data lacks a healthy mirror copy.`
- `fn format_source_io_probe_failure(devid: u64, err: &str) -> String` -- e.g.
  `could not probe source device (devid {devid}) for I/O errors: {err}`.

Both are new top-level fns, so per the project's Doc Comments rule each needs a
short `///` justifying its existence (note: body is prefix-free because
`PreviewNote::Warn` owns the `[warn]` prefix).

### 2. Insert the probe block in `plan_replace`, after source resolution (~line 1302)

Immediately after `let replace_source = match resolve_replace_source(...) {...};`
ends (1301), before `// Probe --new disk state` (1303):

```rust
// Source-read-health note. Pushed as a plan note (not the old
// post-mutation eprintln in execute()) so it renders in --dry-run stdout
// and the pre-confirmation prelude -- before journal write and luksFormat,
// where "is the source healthy enough to read from?" can still inform the
// operator. Live-only: a Missing source has no live device to stat.
if let ReplaceSource::Live { devid, .. } = &replace_source {
    let probe = runner
        .run(&CmdRequest::BtrfsDeviceStatsJson {
            mount_point: config.mount_point().clone(),
        })
        .map_err(|e| e.to_string())
        .and_then(|raw| parse_btrfs_device_stats(&raw).map_err(|e| e.to_string()));
    match probe {
        Ok(stats) if source_has_io_errors(&stats, *devid) => {
            notes.push(PreviewNote::Warn(format_source_io_error_warning(*devid)));
        }
        Ok(_) => {}
        Err(e) => notes.push(PreviewNote::Warn(format_source_io_probe_failure(*devid, &e))),
    }
}
```

The `.map_err().and_then(.map_err())` collapse of runner-error + parse-error
into one `Err` arm follows the established `monitor.rs:79-84` pattern.
`parse_btrfs_device_stats`, `source_has_io_errors`, `CmdRequest`, and
`PreviewNote` are already in scope in `plan_replace`.

### 3. Delete the old probe block in `execute()` (replace.rs:784-799)

Remove the `// Live-only: warn ...` comment and the entire
`if let ReplaceSource::Live { mapper: _, devid } = &replace_source { ... eprintln! ... }`
block. **Keep** line 783's `// Step 2+: Execute replacement ...` comment (it
heads the `let devid = match &replace_source` block at 804, still accurate).
`devid` is independently re-bound at 804, so nothing dangles.

### 4. Doc sync (`docs/commands/replace.md:116`)

Lightly update the existing bullet to reflect that the warning is now a
dry-run-visible plan note shown before confirmation (behavior change per the
doc-sync rule). It stays informational/non-blocking. Check the README
`replace` section; it does not enumerate this warning, so likely no README
change -- verify.

## Tests

Add alongside the keyfile-asymmetry plan-notes tests (~`replace.rs:5897`),
reusing their `warns` filter pattern:
`let warns: Vec<&String> = plan.notes.iter().filter_map(|n| match n { PreviewNote::Warn(b) => Some(b), _ => None }).collect();`

Mock-stats JSON shape is the proven one from the existing unit test at
`replace.rs:2065`:
`{"device-stats":[{"devid":2,"read_io_errs":3,"write_io_errs":0,"flush_io_errs":0,"corruption_errs":0,"generation_errs":0}]}`.
The live `--old disk2` source resolves to **devid 2** (confirmed:
`test_fixtures/replace.rs:25,50`). `MockRunner` resolves handlers in reverse
registration order (`cmd.rs`), so a `.with_handler(...)` layered after
`ReplacementPool::two_disk_healthy().install(...)` overrides the fixture's
default empty-stats handler (`test_fixtures/replace.rs:295`).

**Planner-level tests** (call `plan_replace`, assert on `plan.notes` /
`plan.preview().render()`):

1. **`plan_replace_live_emits_warn_when_source_has_io_errors`** -- install the
   two_disk_healthy live fixture + a `.with_handler` returning the I/O-error
   stats above for devid 2. Assert exactly one `Warn` note equal to
   `format_source_io_error_warning(2)`, and that `plan.preview().render()`
   (uncolored) contains `[warn] source device (devid 2) has I/O errors`.
2. **`plan_replace_live_warns_when_source_stats_probe_fails`** (runner error) --
   same install + a `.with_handler` returning
   `Err(CmdError::Failed("stats unavailable".into()))` for `BtrfsDeviceStatsJson`.
   Assert one `Warn` note starting with the probe-failure prefix
   (`could not probe source device (devid 2) for I/O errors`).
3. **`plan_replace_live_warns_when_source_stats_unparseable`** (parse error;
   finding 2) -- same install + a `.with_handler` returning
   `Ok(RawCommandOutput { stdout: "not json{".into(), exit_status: 0, .. })` for
   `BtrfsDeviceStatsJson`. Assert one `Warn` note with the same probe-failure
   prefix. Pins that *parser drift* surfaces as the non-blocking failure note,
   not a hard error and not a silently dropped warning -- the runner-error test
   (2) alone leaves the parse-error arm uncovered.
4. **`plan_replace_missing_source_skips_io_probe_even_with_dirty_stats`**
   (finding 2) -- `one_live_one_missing` fixture, params
   `.old("disk2").new_disk("disk3=/dev/disk/by-id/virtio-disk3").missing_id(Some(2))`
   (mirrors `replace.rs:5184-5193`), + a `.with_handler` returning **dirty**
   stats for devid 2. Assert (a) no `Warn` note whose body contains
   "I/O errors", and (b) `runner.requests()` contains **zero**
   `BtrfsDeviceStatsJson` -- proving the `ReplaceSource::Live` gate skips the
   probe for a missing source even when a live device would report errors.
5. **(Optional) `plan_replace_live_no_warn_when_source_clean`** -- two_disk_healthy
   with the default empty-stats handler; assert no source-I/O `Warn` note. Mostly
   redundant with the byte-equivalence test below; include only as an explicit
   negative next to the positives.

**Real-run regression** (finding 1 -- guards that the legacy `execute()` block
is actually deleted; a planner/preview-only test passes even if the legacy
`eprintln!` survives, yielding a duplicate, post-mutation warning on real runs):

6. **`cmd_replace_live_source_io_warning_renders_once_on_real_run`** -- mirror
   the accepted-real-run setup at `replace.rs:3259-3283`:
   `PoolFixture::two_disk_healthy()`, `f.confirm.accept()`, runner =
   `ReplacementPool::two_disk_healthy().install(...).with_handler(<dirty stats devid 2>).with_handler(replace_start_fails_handler())`.
   The forced replace-start failure (`replace_start_fails_handler()`,
   `replace.rs:3119`) stops execution *after* the source probe runs in
   `plan_replace` and the note renders at execute() line 444, but avoids
   mocking the full post-replace flow. Wrap the call in the proven capture seam
   (`replace.rs:6057`):
   `let (result, stderr) = super::replace_stderr_capture::capture(|| cmd_replace(&runner, &fs, &f.replace_params().yes(false).build()));`
   Assert:
   - `result.is_err()` (forced replace-start failure);
   - `stderr.matches("[warn] source device (devid 2) has I/O errors").count() == 1`
     -- the note renders exactly once through the real-run path (execute(), not
     just dry-run);
   - **primary deletion guard:**
     `runner.requests().iter().filter(|r| matches!(r, CmdRequest::BtrfsDeviceStatsJson { .. })).count() == 1`.
     `probe_pool` issues no `BtrfsDeviceStatsJson` (confirmed: none in
     `probe.rs`), so the sole probe is `plan_replace`'s; a surviving legacy
     block at 785 makes this 2 and fails the test.

   **Do not** use `!stderr.contains("Warning: source device")` as the deletion
   guard: the legacy line uses raw `eprintln!` (replace.rs:785), which bypasses
   `emit_replace_stderr`/`replace_stderr_capture`, so it never lands in captured
   stderr and the assertion is trivially (misleadingly) true. The probe
   request-count is the only sound guard that the legacy block is gone.

### Existing tests -- impact

None break. The live-path fixtures return a **successful** empty result
(`{"device-stats": []}`), which is clean (no errors) and not a probe failure,
so neither new note path fires for them:

- `plan_replace_live_..._has_no_notes_and_matches_legacy_step_render` (5117):
  clean success -> no note -> byte-equivalence holds.
- `plan_replace_missing_..._has_no_notes...` (5175): `Missing` -> gated out.
- `dry_run_render_fresh_disk_live_replace_with_keyfile` (4067) and sibling
  (4152): call `replace_work_plan_for_test(...).render_steps()` -- render STEPS,
  never construct a `Preview` or touch `notes`. Unaffected.
- `source_has_io_errors` unit test (2065): tests the pure helper. Unaffected.
- keyfile-asymmetry plan-notes tests (~5829, ~5897) with `notes.len() == 1`:
  clean success adds no source note -> count unchanged.

## Verification

1. `just test-rust` -- project's cargo test entry point; runs the full Rust
   suite including the new tests and every `plan_replace`-exercising test above.
2. During iteration: `cargo test -p braid-cli replace` (crate is `braid-cli`).
3. `cargo clippy -p braid-cli` -- catch unused imports/bindings from the
   `execute()` deletion. (Do not run `cargo fmt` -- repo formatting rule.)
4. Manual doc read: confirm `docs/commands/replace.md` bullet reads correctly
   and `mdbook build docs` linkcheck still passes if any link changed.

## Risks

- **Mock JSON field names / devid:** use the exact shape from test 2065 and
  devid 2; a wrong key would parse to zeroed counters (false negative). The
  `warns.len() == 1` + rendered-body assertion guards against a silent
  wrong-for-the-right-reason pass.
- **Assert on uncolored `.render()`** for the `[warn]` prefix; a colored render
  path injects ANSI codes around the prefix.
- **Probe now runs before the capacity check (1343).** A future fixture with
  both dirty source stats and a post-1302 planning failure would surface the
  source note on `PlanFailure::notes` -- intended (operator still learns of
  source I/O errors on an abort), and no current fixture combines the two.
- **Dry-run now issues `btrfs device stats`.** Intended and consistent: the
  keyfile and capacity probes already run in `plan_replace` on dry-run, and
  ADR 022 blesses side-effect-free preview probes. No `[wait]/[ok]` status row
  is warranted (instantaneous in-memory read, like its sibling probes).
