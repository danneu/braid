# Plan: make `braid status` config-probe errors non-fatal

## Context

`braid status` is braid's always-available read-only diagnostic. principles.md
makes that an invariant: *"Read-only diagnostics `status` and `doctor` ... so
operators retain a working diagnostic surface"* and lists `status` among the
surfaces that "stay available" even during contention/recovery.

Today it violates that. In `build_status`, every configured member is probed
with `probe_config_disk`, and the results are collected with
`.collect::<Result<Vec<_>, _>>()?` (`cli/src/status.rs:571-582`). For a member
that is *present and healthy in the live pool*, the returned `ConfigDisk` is
discarded (the present rows come from the UUID-keyed membership join per
decision-024; the unpooled loop `continue`s past live members). The probe's
*only* effect for such a member is its error side-channel: a `MapperConflict`,
`MapperBackingMismatch`, `MapperBackingResolveError`, `UnsupportedLuksVersion`,
or `Cmd`/`Parse` failure on the *expected* `braid-<name>` mapper or the by-id
handle short-circuits `build_status` via `?`. That propagates to `cmd_status`
(`status.rs:650`, before the `if json` branch) and `main.rs:698-708`
(`print_cli_error` + `exit(1)`), so **both human and JSON output are blanked**
-- no pool summary, no capacity, no per-disk detail -- even when the pool is
mounted and healthy via a drifted mapper. The pinned test
`status_surfaces_mapper_conflict` (`status.rs:6571`) currently *enforces* this
hard-fail.

This contradicts (a) the principle above, (b) `doctor`, the sibling read-only
diagnostic, which deliberately degrades (`probe_pool` stored as a tolerant
`Result` at `doctor.rs:713`; it never calls `probe_config_disk`), and (c)
decision-024, which says present-member identity comes from the UUID join and
that **mapper/label drift is tolerated** -- a hijacked `braid-disk1` for a
member already live-and-healthy under another mapper is the textbook
tolerated-drift case, not a reason to refuse all diagnostics.

This plan keeps the full probe sweep (its *error* is still the only status-side
surface for live-member mapper/backing/LUKS faults -- the reason commit
`77e94a47` kept it) but makes that error **non-fatal**: a per-disk advisory
plus, for an unpooled member, a cause-neutral `Unknown` row. A peripheral
mapper hijack degrades one member instead of blanking the whole report.

### Why localize to status (not a new `ConfigDiskState` variant)

`probe_config_disk` is also called by the mutating/unlock gateways
(`add`, `replace`, `enroll_key_file`, `mount::plan_open_pool`, `recover` x7),
which *legitimately* fail closed on these errors, and `ConfigDiskState`'s three
variants are matched exhaustively in destructive-format guards
(`add.rs:1988/2105`, `replace.rs:1423-1428/1449/1522/1706+`). Adding a
`ConfigDiskState::ProbeFailed` variant would force a safety decision into every
one of those guards for a status-only display concern. **The fix must touch only
status's call site; `probe_config_disk` and `ConfigDiskState` stay byte-for-byte
unchanged, and every fail-closed gateway keeps its current behavior.**

## Approach

All edits in `cli/src/status.rs` plus a small doc note. No changes to
`probe.rs`, `types.rs`, or any mutating command.

### 1. `build_status`: replace the fatal collect with a per-member loop

Replace `cli/src/status.rs:571-582` (the `.collect::<Result<Vec<_>, _>>()?`)
with an explicit loop that partitions probe outcomes:

```rust
let mut config_disks: Vec<ConfigDisk> = Vec::with_capacity(members.len());
let mut probe_failures: Vec<ConfigProbeFailure> = Vec::new();
for member in members {
    match probe_config_disk(runner, fs, &member.name, &member.by_id, backing_path_resolver) {
        Ok(cd) => config_disks.push(cd),
        Err(e) => {
            advisories.push(config_probe_advisory(&member.name, &e));
            probe_failures.push(ConfigProbeFailure {
                name: member.name.clone(),
                by_id: member.by_id.clone(),
            });
        }
    }
}
```

`advisories` already exists (`status.rs:481`, type `Vec<String>`, serialized as
the existing `advisories[]` JSON field with `skip_serializing_if = Vec::is_empty`,
rendered human-side at `status.rs:1285-1287` as `warning: {advisory}`). Nothing
new is needed for advisory plumbing.

Rewrite the comment block at `status.rs:562-570` to state the new contract:
the sweep stays (its error is the only status-side live-member fault surface),
but unlike the mutating gateways that fail closed, **status is the
always-available read-only diagnostic (principles.md), so a probe error here is
non-fatal** -- it becomes an advisory plus, for an unpooled member, an `Unknown`
row.

### 2. New status-local helper + struct

```rust
/// Phrase a config-disk probe failure as a status advisory. The mapper- and
/// LUKS-version `ProbeError` variants already embed the disk name and the
/// remediation command in their `Display`, so they pass through verbatim; the
/// environmental `Cmd`/`Parse` variants do not name the disk, so they are
/// attributed here. Invariant: every config-probe advisory names its disk
/// (pinned by `config_probe_advisory_names_disk`).
fn config_probe_advisory(name: &DiskName, e: &ProbeError) -> String {
    match e {
        ProbeError::MapperConflict { .. }
        | ProbeError::MapperBackingMismatch { .. }
        | ProbeError::MapperBackingResolveError { .. }
        | ProbeError::UnsupportedLuksVersion { .. } => e.to_string(),
        _ => format!("disk '{name}' probe failed -- {e}"),
    }
}

/// A configured member whose `probe_config_disk` errored during status. Carries
/// the identity needed to render a cause-neutral `Unknown` row without a
/// `ConfigDisk` -- status keeps probe errors non-fatal while the gateway role
/// stays in the mutating commands.
struct ConfigProbeFailure {
    name: DiskName,
    by_id: ByIdPath,
}
```

The advisory string is ASCII (`--`, `'`), satisfying `check-output-ascii.py`.

### 3. `build_disk_reports`: render Unknown rows for unpooled failures

Add a `probe_failures: &[ConfigProbeFailure]` parameter (signature at
`status.rs:991-997`; update the call at `status.rs:591` to pass
`&probe_failures`).

The unpooled detail rows must stay sorted by `DiskName`
(decision-024#consequences: *"Display surfaces that need stable operator
ordering must sort by `DiskName`"*). The present block already sorts at
`status.rs:1016`, and the Ok-unpooled rows are name-ordered today only because
`iter_by_name` pre-sorts them (`membership.rs:312-316`). A probe failure and a
successfully-classified unpooled member must therefore **interleave by name**,
not concatenate. So refactor the unpooled emission to collect *both* kinds into
one local vec, sort once, then drain into the report vectors. Replace the
existing direct pushes in the unpooled loop (`status.rs:1149-1176`) with pushes
into a local `unpooled` vec:

```rust
// Both kinds of unpooled row, paired and keyed by name for a single ordered block.
let mut unpooled: Vec<(DiskReport, HumanDisk)> = Vec::new();

// (existing) successfully-classified unpooled members: same per-`cd.state`
// classification as today (status.rs:1102-1148), but pushed into `unpooled`.
for cd in config_disks {
    if membership.by_name(&cd.name).is_some_and(|(uuid, _)| pool_uuid_set.contains(uuid)) {
        continue;
    }
    // ... unchanged (status, luks_uuid) classification ...
    unpooled.push((DiskReport { /* as today */ }, HumanDisk { /* as today */ }));
}

// (new) probe failures with no ConfigDisk to classify. A live member is already
// rendered Present by the present loop above; the build_status advisory carries
// the fault either way. A non-live member gets a cause-neutral Unknown row so it
// is not silently dropped and the compact summary (which mirrors these detail
// verdicts) does not fall back to Missing.
for failure in probe_failures {
    if membership.by_name(&failure.name).is_some_and(|(uuid, _)| pool_uuid_set.contains(uuid)) {
        continue;
    }
    let mapper = mapper_name(&failure.name).0;
    unpooled.push((
        DiskReport {
            name: failure.name.as_str().to_owned(),
            mapper,
            by_id: failure.by_id.as_str().to_owned(),
            luks_uuid: String::new(),
            devid: None,
            underlying: None,
            status: DiskStatus::Unknown,
            btrfs_errors: None,
            smart: None,
        },
        HumanDisk {
            name: failure.name.as_str().to_owned(),
            member_name: Some(failure.name.clone()),
            by_id: failure.by_id.as_str().to_owned(),
            luks_uuid: String::new(),
            devid: None,
            status: DiskStatus::Unknown,
            model: None,
            serial: None,
            errors: None,
            smart: None,
        },
    ));
}

// Single name-sorted unpooled block (mirrors the present block's sort at
// status.rs:1016). Idempotent for the Ok-only case, so existing output stays
// byte-identical when there are no failures.
unpooled.sort_by(|(a, _), (b, _)| a.name.cmp(&b.name));
for (report, human) in unpooled {
    disk_reports.push(report);
    human_details.push(human);
}
```

The failure rows mirror the existing unpooled row construction
(`status.rs:1149-1176`) and reuse the same `pool_uuid_set` the unpooled loop
already computes. `Unknown` is the right vocabulary:
`absent_data_reason(Unknown)` = `"metadata unavailable"` (`status.rs:1221`) and
it carries no action hint -- decision-024 defines `unknown` as "braid cannot
classify the state." The combined sort guarantees JSON `disks[]` and human
detail stay in operator-name order regardless of which members failed to probe.

**Why the Unknown row must live here, not in `build_status`:**
`build_compact_drives` (`status.rs:304-323`) looks up each member's status from
`member_status` (derived from `verbose_ctx.disks` at `status.rs:597-601`) and
**falls back to `Missing` when there is no detail row**. Emitting the `Unknown`
detail row inside `build_disk_reports` is what keeps the compact summary and the
detail section in agreement -- the invariant the comment at `status.rs:593-596`
demands. A present (live) failed member is skipped here and rendered `Present`
by the present loop; its compact row stays `Present` too. Consistent on both
paths.

### 4. Tests (unit only -- four behavioral tests)

- **Rewrite `status_surfaces_mapper_conflict` (`status.rs:6556-6706`).** Same
  fixture (healthy 1-disk pool live under mapper `disk1`; `braid-disk1`
  externally aliased to UUID `9999...` -> `MapperConflict`). Change the
  assertions from `Err(StatusError::Probe(MapperConflict))` to: `build_status`
  returns `Ok`; `report.status == StatusCode::Intact` and `present_count == 1`
  (pool not blanked); `report.advisories` contains an entry naming `disk1` and
  the "not backed by the configured disk" text; the `disk1` row renders
  `DiskStatus::Present` (from the pool join). Update the preamble: intent
  becomes "a MapperConflict on a present member's expected mapper surfaces as a
  non-fatal advisory while the healthy pool still renders"; why-it-exists
  becomes "a regression could reintroduce the `?` that blanks the
  always-available read-only diagnostic." Keep the scenario. This preserves the
  test's original purpose (the fault must reach the non-mutating boundary, not
  be swallowed) and states it more precisely than the old `Err` assertion.

- **Add `status_unpooled_probe_failure_renders_unknown`.** A degraded/offline
  member whose by-id probe errors (e.g. `MapperBackingResolveError` or a
  `luksDump` parse error). Assert `build_status` returns `Ok`, an advisory names
  the member, and that member's detail row is `DiskStatus::Unknown` (not
  `Missing`, not absent), with the compact drive mirroring `Unknown`.

- **Add `config_probe_advisory_names_disk`.** Construct one representative of
  each `ProbeError` variant `probe_config_disk` can return for a present member
  (`Cmd`, `Parse`, `MapperConflict`, `MapperBackingMismatch`,
  `MapperBackingResolveError`, `UnsupportedLuksVersion`) and assert each
  advisory string contains the disk name. Pins the helper's invariant against
  future variant or `Display` drift.

- **Add `status_unpooled_rows_sorted_by_name_across_ok_and_failures`** (the
  mixed success/failure case F1 requires). Configure >=3 unpooled members whose
  name order interleaves probe outcomes -- e.g. member `a` probe-fails
  (-> Unknown), `b` classifies Ok (`Absent` -> Missing, or verified -> Offline),
  `c` probe-fails (-> Unknown). Assert the rendered `disks[]` / human detail
  rows appear in name order `a, b, c`, not the Ok row followed by the two
  failures. Pins the combined sort against regression to an append-after shape.

Reuse existing helpers: `isolated_paths`, `MockRunner.with_output`,
`status_config`, `status_fs_one_disk`, `MockBackingPathResolver`,
`save_membership`, and the `status_*` fixtures in
`cli/src/test_fixtures/status.rs`.

### 5. Docs

- **`docs/commands/status.md`** (JSON envelope + human-output sections): state
  that a config-side probe fault **always** adds a `warning:` line /
  `advisories[]` entry and that `status` stays non-fatal (exit 0) -- it does not
  abort like the mutating commands; and that **only an affected member that is
  not live in the pool** additionally gets an `unknown` disk row. A live member
  keeps its `present` row from the pool UUID join and is flagged by the advisory
  alone -- do not imply every probe fault adds an `unknown` row.
- **`plans/impl/2026-05-26-status-config-disk-probe-rationale.md`**: add a
  one-line note at the top recording that the *fatality* of the present-member
  probe described there was revisited (status now treats config-probe errors as
  non-fatal), pointing to this plan. Preserve the rest as history; do not rewrite
  it.
- No `principles.md` or decision-024 edit: the fix makes the code *honor* the
  existing "status stays available" invariant and is consistent with 024's
  tolerated-drift / `unknown` semantics. Cite both in the code comment.

## What deliberately stays the same

- The full probe sweep (every member is still probed). Only the *fatality*
  changes; the I/O the prior rationale defended is untouched.
- `probe_config_disk`, `ProbeError`, `ConfigDiskState`, and every mutating/unlock
  gateway's fail-closed handling -- byte-for-byte unchanged.
- `Absent`/`Missing` behavior: an absent by-id path is still `Ok(Absent)` ->
  `Missing` row; only genuine probe *errors* take the new advisory path.
- The healthy-pool happy path: no probe error -> no advisory -> output is
  byte-identical to today.

## Verification

1. `just test-rust` -- CLI unit tests, including the rewritten
   `status_surfaces_mapper_conflict` and the three new tests. The healthy-pool
   tests (`cmd_status_healthy_*`, `build_status_*`) must still pass unchanged
   (happy path is byte-identical).
2. `cargo clippy --manifest-path cli/Cargo.toml --tests` -- no unused-binding
   warnings from the new loop/struct.
3. `python3 scripts/docs/check-output-ascii.py` -- confirms the new advisory
   string is ASCII.
4. `just docs-build` -- `mdbook-linkcheck2` validates the `status.md` edit.
5. Manual sanity (optional): `braid status` on a healthy pool renders
   identically to before; with `braid-<name>` aliased to a foreign container, it
   now exits 0, prints the pool summary, and shows the `warning:` advisory.

## Critical files

- `cli/src/status.rs` -- all code edits: probe loop + comment (`~562-582`),
  `config_probe_advisory` + `ConfigProbeFailure` (new), `build_disk_reports`
  signature + combined name-sorted unpooled emission (`~991`, `~1149-1179`),
  call site (`~591`), rewritten test + three new tests (`~6556`).
- `docs/commands/status.md` -- advisory/disk-status note.
- `plans/impl/2026-05-26-status-config-disk-probe-rationale.md` -- supersession
  note.
- Reference only (confirm unchanged): `cli/src/probe.rs` (`probe_config_disk`,
  `ProbeError`), `cli/src/types.rs` (`ConfigDiskState`), `cli/src/doctor.rs:713`
  (tolerant sibling), `docs/design/principles.md` (status-availability
  invariant), `docs/design/decisions/024-luks-uuid-identity.md`.

## Implementation notes

- `status_unpooled_rows_sorted_by_name_across_ok_and_failures` (test F1) is
  written against `build_disk_reports` directly with a hand-built
  `probe_failures` slice rather than through `build_status`. The combined sort
  is wholly contained in `build_disk_reports`, so a direct call pins it without
  the full mounted-status mock; `status_unpooled_probe_failure_renders_unknown`
  covers the `build_status` advisory + compact plumbing end-to-end.
- `status_unpooled_probe_failure_renders_unknown` triggers the unpooled probe
  error by leaving disk2's `CryptsetupLuksUuid` unmocked (-> `MissingMock` ->
  `ProbeError::Cmd`), which also exercises the `config_probe_advisory`
  Cmd/Parse-variant attribution path (the disk name is not self-named by that
  variant's `Display`). The plan listed `MapperBackingResolveError` / a luksDump
  parse error only as examples.
- The supersession note added to
  `plans/impl/2026-05-26-status-config-disk-probe-rationale.md` links this plan
  by its promoted path `2026-06-09-status-config-probe-non-fatal.md`.
