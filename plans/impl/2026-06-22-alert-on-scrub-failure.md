# Alert on btrfs scrub-service failure

## Context

braid runs a periodic maintenance scrub (`braid-scrub.service`, default monthly) but
wires up **no failure alerting**. A scheduled scrub that fails to run or complete
(btrfs internal error, transient device error that aborts the scrub, ENOSPC on
metadata, a spawn failure) today just leaves the systemd unit in `failed` state with
**no operator-facing signal at all** -- the device-stats monitor (`braid monitor`)
polls `btrfs device stats`, which a *failed-to-run* scrub never moves, so it stays
silent. The goal is to make a genuinely failed scrub raise braid's existing
user-facing alert (`braid-alert.service`) via `onFailure`.

The difficulty is entirely in three nuances, all confirmed against the code/source:

1. **Spurious alerts on normal teardown.** `braid-scrub.service` is routinely stopped
   mid-scrub by ordinary lifecycle events (`braid lock`, suspend, shutdown). Its
   `ExecStop` cancels the in-flight scrub via the `BTRFS_IOC_SCRUB_CANCEL` ioctl.
   When a **real** scrub is cancelled, btrfs-progs exits **1** -- in
   `reference/btrfs-progs/cmds/scrub.c` `scrub_start`, an `ECANCELED` device result
   does `++err`, and the function ends with `if (err) return 1`. `cmd_scrub_resume_or_start`
   maps exit 1 to `Err` (`ResumeFailed`/`StartFailed`), so the unit lands in `failed`
   -- the **same** outcome as a genuine failure. A naive `onFailure` would therefore
   beep on every lock/suspend/shutdown. The existing cancel test hides this by faking
   the scrub with `sleep 300` (killed by SIGTERM, which is clean for `Type=simple`),
   so the real-cancel-exit-1 path is currently **untested**. Worse, btrfs sets
   `canceled = !!ret` for *any* nonzero scrub ioctl (`scrub.c#scrub_one_dev`), so a
   fatal scrub error renders as `aborted` too -- scrub status cannot distinguish a
   deliberate cancel from a genuine failure. The discriminator must be an explicit
   *cancel-requested* marker that braid's own teardown writes, not btrfs status.

2. **Scope: failure vs. corruption.** A scrub that finds uncorrectable errors makes
   `cmd_scrub_resume_or_start` return `Ok(...uncorrectable_errors: true)`, but
   `main.rs#Commands::ScrubResumeOrStart` then **exits 3** (btrfs parity). systemd
   treats exit 3 as failure unless told otherwise, so once `onFailure` is wired the
   corruption case would fire `ScrubFailed` -- yet
   [ADR 014](../../docs/design/decisions/014-alerts.md) says scrub-found corruption
   already reaches the operator via the device-stats poll's `BtrfsDeviceErrors` cause,
   and that *"a separate scrub-status alert probe would be redundant with this
   pipeline."* Decision (below): declare exit 3 a service success
   (`SuccessExitStatus=3`); `onFailure` covers **scrub-failed-to-run/complete only**,
   never corruption.

3. **Ack/latch for a non-counter source.** braid's alert model
   ([ADR 014](../../docs/design/decisions/014-alerts.md)) latches causes in
   `alert-latch.json`, surfaces them in `braid status`, and clears them with
   `braid ack`. A scrub-failure has no device counter. The chosen design models it on
   the **smartd alert source**, which is an exact precedent for a non-counter event:
   a flag file -> immediate beep + the monitor latches a cause -> `braid ack` clears
   flag+latch+beep.

**Chosen scope (confirmed):** full latch integration (new `AlertCause::ScrubFailed`,
`braid status` names it, `braid ack` clears it), gated on `monitor.enable`, plus a
build-time warning for the `autoScrub.enable && !monitor.enable` combination.

**Severity tier (post-ENOSPC rebase).** Commit `cf28ce7f` gave every `AlertCause` a
severity (`AlertSeverity::{Warning, Critical}`, ADR 014
[#severity-tiers-and-the-enospc-baseline](../../docs/design/decisions/014-alerts.md#severity-tiers-and-the-enospc-baseline)):
the monitor's exit code now branches on `AlertState::severity()` (Critical -> exit 1 ->
`braid-alert.service` beeps; the lone Warning cause `EnospcRisk` -> exit 3 ->
`braid-alert-advisory.service`, `alertCommand` only). `ScrubFailed` is **Critical** -- it
beeps, exactly like the smartd source it mirrors -- so it joins the Critical tier, never the
non-beeping Warning/advisory path. This is a wording/classification rebase, not a redesign:
`onFailure` + a durable flag is unchanged.

## Authority to read first

- [ADR 018 systemd-lifecycle](../../docs/design/decisions/018-systemd-lifecycle.md) --
  `#braid-scrubtimer--scrub-service--resume-trigger----lifecycle-bound-scrub`,
  `#braid-alertservice--notification`, and the canonical exit-code table
  `#braid-monitortimer--braid-monitorservice--health-polling`.
- [ADR 014 alerts](../../docs/design/decisions/014-alerts.md) -- `#alert-causes`,
  `#two-detection-sources-one-alert-model`, `#offline-ack-policy`, and
  `#severity-tiers-and-the-enospc-baseline` (the Warning/Critical model `ScrubFailed`
  must slot into).
- [safety-heuristics.md](../../docs/dev/safety-heuristics.md) (fail-closed policy),
  [testing.md](../../docs/dev/testing.md) (VM-test preamble + `flake.nix` registration).

---

## Change 1 -- Rust: cancel-vs-failure disambiguation via a cancel-request marker (nuance #1)

**Files:** `cli/src/scrub_resume_or_start.rs`, `cli/src/state_paths.rs`,
`modules/braid/storage.nix#scrubCancelScript`, `cli/src/main.rs`.

btrfs exits 1 for *both* a deliberate cancel and a genuine fatal scrub error, and
`scrub.c#scrub_one_dev` sets `canceled = !!ret` for **any** nonzero scrub ioctl, so the
rendered scrub status (`aborted`) cannot tell them apart. The only authoritative signal
for "this was a deliberate teardown" is braid's own intent, so the teardown path records
a **cancel-request marker** and the scrub runner keys off it:

- `cli/src/state_paths.rs` (`StatePaths`): add `scrub_cancel_requested()` ->
  `/var/lib/braid/scrub-cancel-requested`. Ephemeral per-run coordination (not a durable
  alert flag); kept in the state dir so `StatePaths::custom` relocates it under a temp dir
  for unit tests. Path literal shared with the ExecStop shell script, same as the smartd
  flag is shared between `smartdAlertScript` and `smartd_alert()`.
- `modules/braid/storage.nix` (`storage.nix#scrubCancelScript`): `touch` the marker as the
  **first** action of the ExecStop script, before the `mountpoint -q` early-exit and the
  `braid scrub-cancel` ioctl, so it is present on every deliberate stop (`braid lock`,
  suspend, shutdown, mount-gone race).
- `cli/src/scrub_resume_or_start.rs` (`#cmd_scrub_resume_or_start`):
  - At entry, **remove** any stale marker so only a cancel requested *during this run*
    counts -- and treat the removal **fail-closed** (per
    [safety-heuristics.md](../../docs/dev/safety-heuristics.md#mutation-safety-heuristics):
    *"set fail-closed policy from the downstream failure mode ... every uncertainty in that
    branch is a hard error"*). Tolerate **only** `io::ErrorKind::NotFound` (nothing to
    clear, the common between-runs case). **Any other** removal error -- the path is a
    directory, `EACCES`, `EIO`, etc. -- returns `Err` **before btrfs runs**: the scrub does
    not start and the unit fails (-> `onFailure` -> `ScrubFailed`). Rationale: if entry
    cleanup cannot *guarantee* a clean slate, a surviving stale marker would later turn a
    *genuine* exit 1 into `Ok(Cancelled)` and silently swallow the very failure this feature
    exists to alert on. The downstream failure mode (an un-alerted scrub failure) makes
    every cleanup uncertainty a hard error, even though the sibling "no marker" case
    proceeds.
  - On a btrfs exit outside `{0,2,3}` (today the `_ => Err(...)` arm in both the resume
    and start matches): marker present -> `Ok(ScrubResumeOrStartResult::Cancelled)` (clean;
    service exits 0); marker absent -> `Err` (genuine failure; service exits non-zero,
    fires `onFailure`). Test presence with `Path::exists()`, which coerces any I/O error to
    `false`, so the **only** route to `Cancelled` is an unambiguously present marker;
    absence *or* any read ambiguity falls through to `Err` -> alert (fail-closed on this arm
    too). **Do not** consult btrfs scrub status -- per F2 it cannot distinguish cancel from
    failure. The marker is the sole discriminator.
- Ordering is race-free: the entry-remove runs when the scrub first starts (long before
  any stop); ExecStop writes the marker *before* issuing the cancel that makes btrfs
  return 1; so the marker is present at the runner's post-exit check **iff** a stop is in
  flight for this run.

Supporting edits:

- Add `ScrubResumeOrStartResult::Cancelled` to the result enum, plus a new
  `ScrubResumeOrStartError::MarkerCleanupFailed { source: std::io::Error }`
  (`#[error("could not clear stale scrub-cancel marker: {source}")]`) so the journal names
  the real problem -- per [safety-heuristics.md](../../docs/dev/safety-heuristics.md#mutation-safety-heuristics)
  *"split ... failure variants by the operator's remediation,"* the remediation here
  (inspect the poisoned `scrub-cancel-requested` path) is distinct from a btrfs
  resume/start failure. Factor the marker remove-at-entry + check-on-error into one helper
  used by both arms. The marker path is injected (via `StatePaths`) so MockRunner unit
  tests exercise every arm against a temp file.
- `cli/src/main.rs` (`#Commands::ScrubResumeOrStart`): add an `Ok(Cancelled)` arm ->
  `std::process::exit(0)` with an informational log (e.g. `scrub cancelled (resumable)`);
  every `Err` (including the new `MarkerCleanupFailed`) still `exit(1)`, so a poisoned
  marker fails the unit and its message surfaces in the journal -- no new arm needed; the
  existing `uncorrectable_errors: true` arm still `exit(3)` (now whitelisted by
  `SuccessExitStatus`, Change 3).
- Update the `cmd_scrub_resume_or_start` doc comment: describe the marker discrimination
  and that exit 3 stays a service success per ADR 014.

---

## Change 2 -- Rust: `ScrubFailed` alert source (nuance #3)

Mirror the smartd source end to end. New flag file: `/var/lib/braid/scrub-failed`.

- `cli/src/state_paths.rs` (`StatePaths`): add `scrub_failed()` -> `/var/lib/braid/scrub-failed`
  (mirror `smartd_alert()`).
- `cli/src/alert.rs`:
  - Add unit variant `AlertCause::ScrubFailed` to `alert.rs#AlertCause`. **No** `Display`
    code and **no** JSON code: `AlertCause` has no `Display` impl (the human text lives in
    `status.rs#format_status_human`, below), and the enum is
    `#[serde(tag = "type", rename_all = "snake_case")]`, so the variant auto-serializes as
    `{"type":"scrub_failed"}` -- a new `--json` output-shape value to *document*, not write.
  - Classify the variant in `alert.rs#AlertCause::severity` as **`Critical`**: add
    `ScrubFailed` to the existing
    `BtrfsDeviceErrors | MissingDevice | SmartdAlert | ComputationError => AlertSeverity::Critical`
    arm. That match is **exhaustive** (the only `Warning` is `EnospcRisk`), so the build breaks
    until the variant is classified -- this is not optional polish. Critical is the only
    correct tier: a failed scrub must beep, exactly as the smartd source it mirrors does.
    `Warning` would route a latched `ScrubFailed` to exit 3 -> `braid-alert-advisory.service`
    (`alertCommand` only, no beep), silently contradicting Change 3's
    `braid-scrub-failed.service`, which starts the Critical beeper (`braid-alert.service`) on
    `onFailure`. `AlertState::severity()` (the per-cycle max over causes) then reports exit 1
    for any cycle that carries `ScrubFailed`, so `main.rs#Commands::Monitor` takes the
    `Critical => exit 1` branch and `braid status` renders the `ALERT` banner, not the
    Warning `NOTICE` capacity-risk banner. (`compute_alert_state` pushes `ScrubFailed` via the
    smartd-style bool; `EnospcRisk`'s separate append path in `cmd_monitor` is untouched.)
  - Add the `(ScrubFailed, ScrubFailed) => true` arm to `alert.rs#same_cause_key` so
    `merge_into_latch` keeps a **single** `ScrubFailed` slot. Without it the pair falls to the
    existing `_ => false`, which *compiles* but makes every monitor cycle append a fresh
    duplicate -- the flag persists from `onFailure` until ack, so the latch grows unbounded.
    (Regression-tested below; this is the most likely silent bug in the change.)
  - Add `scrub_failed_active(paths)` and `remove_scrub_failed_flag(paths)` (mirror
    `alert.rs#smartd_alert_active` / `alert.rs#remove_smartd_alert_flag`; regular-file check).
  - Add a `scrub_failed: bool` parameter to `alert.rs#compute_alert_state` (parallel to
    the existing smartd bool) that pushes `ScrubFailed`. Update all call sites.
- `cli/src/monitor.rs` (`monitor.rs#cmd_monitor`): read `alert::scrub_failed_active(paths)`
  beside the existing smartd-flag read and pass it to `compute_alert_state`, so a present
  flag latches `ScrubFailed`.
- `cli/src/status.rs`:
  - `status.rs#resolve_alert_state`: push `ScrubFailed` from the flag (mirror the smartd
    push) so `braid status` names the cause **immediately**, before the monitor's next poll.
  - `status.rs#format_status_human`: add a `ScrubFailed` arm to the **exhaustive** `match
    cause` (it has no `_` wildcard, so the build breaks until the arm exists). This is where
    the operator-facing text is chosen -- e.g. `  - scheduled scrub failed -- check
    journalctl -u braid-scrub.service` (ASCII per AGENTS.md, matching the existing
    `  - SMART health warning` style).
- `cli/src/ack.rs` -- `ScrubFailed` is a **fall-through** cause like `SmartdAlert`: it gets
  **no** dedicated arm (there is none for `SmartdAlert` either), no `BtrfsDeviceErrors`-style
  offline refusal, and no `MissingDevice`-style acked-stats filter. What actually gates a bare
  flag is a `scrub_failed_active` term threaded everywhere `smartd_active` already flows:
  - `ack.rs#cmd_ack_impl` entry: snapshot `alert::scrub_failed_active(paths)` beside the
    smartd snapshot (ADR 014 "Ack snapshots gating inputs"); derive
    `remove_scrub_failed = scrub_failed_active || latch_had_scrub_failed` once, mirroring
    `remove_smartd`.
  - Add the `scrub_failed_active` term to **both** empty-latch gates that today read
    `causes.is_empty() && !smartd_active && !latch_corrupt` -- the hoisted cleanup-only retry
    gate and the mounted no-alert short-circuit -- or a bare `ScrubFailed` flag is skipped on a
    mounted pool.
  - `ack.rs#ack_offline`: add a `scrub_failed_active` parameter and include it in
    `has_alert = !causes.is_empty() || smartd_active || scrub_failed_active || latch_corrupt`,
    so a bare-flag offline ack proceeds instead of returning `PoolNotMounted`. `ScrubFailed`
    falls through the `BtrfsDeviceErrors` refusal and the `MissingDevice` filter unchanged (no
    new arm), exactly as `SmartdAlert`/`ComputationError` do -- so offline ack removes the flag
    and writes no `acked-stats.json`.
  - `ack.rs#cleanup_alert_files_and_beeper`: add a `remove_scrub_failed: bool` parameter
    parallel to `remove_smartd`, threaded from both call sites, and call
    `alert::remove_scrub_failed_flag(paths)` alongside the smartd-flag removal (before the
    latch; the corrupt sidecar stays last per ADR 014's forensic order).
  - No change to `ack.rs#stop_beeper` -- post-ENOSPC it now stops **both** alert units
    (`braid-alert.service`, the Critical beeper, *and* `braid-alert-advisory.service`, the
    non-beeping Warning advisory), so one ack silences every tier regardless of which source
    or severity started it. `ScrubFailed` is Critical and surfaces through
    `braid-alert.service`, which `stop_beeper` already stops -- nothing to add to the stop
    list. (The surrounding cleanup shape is also current: `cmd_ack_impl` snapshots the gating
    flags, `cleanup_alert_files_and_beeper` brackets the destructive removals with the
    `alert-cleanup-pending` sentinel, and `stop_beeper` runs first/best-effort per ADR 014's
    cleanup invariants.)

---

## Change 3 -- Nix module: `onFailure` wiring + warning (nuances #1/#2)

- `modules/braid/monitor.nix`: add `systemd.services.braid-scrub-failed` (oneshot) inside
  the existing `config = lib.mkIf (cfg.enable && cfg.monitor.enable)` block, modeled
  exactly on `monitor.nix#smartdAlertScript`:
  ```nix
  systemd.services.braid-scrub-failed = {
    description = "Record and announce a failed braid scrub";
    serviceConfig.Type = "oneshot";
    script = ''
      touch /var/lib/braid/scrub-failed
      ${pkgs.systemd}/bin/systemctl start braid-alert.service 2>/dev/null || true
    '';
  };
  ```
  (Durable flag for status/latch/ack **and** the immediate beep, exactly as the smartd
  hook does. No `ConditionPathIsMountPoint` -- it must run even if the failure is a lost
  mount.)
- `modules/braid/storage.nix` (`storage.nix#braid-scrub`):
  - Unit-level: `onFailure = lib.mkIf cfg.monitor.enable [ "braid-scrub-failed.service" ];`
    -- gated so there is no dangling unit reference when the monitor (hence
    `braid-scrub-failed.service` + `braid-alert.service`) does not exist.
  - `serviceConfig.SuccessExitStatus = [ 3 ];` -- declare btrfs's exit 3 (uncorrectable
    errors found, scrub completed) a **success**, so corruption never reaches `onFailure`
    (it alerts via the monitor's `BtrfsDeviceErrors` poll per ADR 014). `main.rs` exits 3
    for that case today, so without this the corruption path would fire `ScrubFailed`.
    NOT gated on `monitor.enable` -- harmless and correct regardless, and it also fixes a
    latent today-bug where an uncorrectable-error scrub silently leaves the unit `failed`.
    Only exit 3 is whitelisted; genuine failures (exit 1) still fail the unit.
- `modules/braid/options.nix` (`options.nix#assertions`, top-level config): add a
  `warnings` entry (the `lib.optional` idiom already used in
  `modules/braid/fan-control.nix`) for the footgun:
  ```nix
  warnings = lib.optional (cfg.enable && cfg.autoScrub.enable && !cfg.monitor.enable) ''
    braid: autoScrub is enabled but monitor is disabled -- scrub failures and
    scrub-discovered corruption will not raise any alert (no beep, no `braid status`
    cause). Enable braid.monitor to alert on scrub problems.
  '';
  ```
  A warning, not an assertion: a "run my own monitoring" setup is unusual but legitimate
  and must not fail to evaluate.

---

## Change 4 -- Docs / ADRs

- [ADR 018](../../docs/design/decisions/018-systemd-lifecycle.md):
  - In the scrub-unit section, document `onFailure = [ braid-scrub-failed.service ]`
    (gated on `monitor.enable`), `SuccessExitStatus=3`, and the **clean-teardown
    contract**: a cancelled scrub makes btrfs exit 1 (indistinguishable from a real
    failure by exit code *or* scrub status, since `canceled = !!ret`), so braid's ExecStop
    writes a cancel-request marker that `scrub-resume-or-start` keys off to exit 0 on a
    deliberate stop; and exit 3 (uncorrectable errors) is declared a success so corruption
    routes to the monitor, not `onFailure`. lock/suspend/shutdown therefore never leave the
    unit `failed`. State that the marker + `SuccessExitStatus=3` are what make `onFailure`
    safe, and that the marker discipline is **fail-closed**: if entry cleanup cannot
    guarantee a clean slate (un-removable marker), the run errors out and alerts rather than
    risk reading a later genuine exit 1 as a cancel.
  - In `#braid-alertservice--notification`, note the new `braid-scrub-failed.service`
    start source.
  - In the canonical exit-code table
    (`#braid-monitortimer--braid-monitorservice--health-polling`, which ADR 018 now owns),
    extend the **exit 1 / Critical** row's cause list to name a latched `ScrubFailed`
    alongside btrfs device errors / missing device / SMART / `ComputationError`. No new exit
    code -- `ScrubFailed` is Critical, so it reuses the existing exit-1 -> `braid-alert.service`
    row; the exit-3 -> `braid-alert-advisory.service` (`EnospcRisk`) row is untouched.
- [ADR 014](../../docs/design/decisions/014-alerts.md):
  - Add `ScrubFailed` to `#alert-causes` -- the periodic scrub failed to run/complete,
    **distinct** from scrub-found corruption (which still alerts via `BtrfsDeviceErrors`).
  - Extend "Two detection sources" to include the scrub-failed flag as a third event
    source feeding the latch; reaffirm that the "redundant scrub-status probe" note
    applies to *corruption* only -- `ScrubFailed` covers *execution failure*.
  - Add `ScrubFailed` to the **Critical** tier in
    `#severity-tiers-and-the-enospc-baseline` **and** to the Critical cause enumeration in
    the pure-detector/beep-semantics section (the existing "`BtrfsDeviceErrors`,
    `MissingDevice`, `SmartdAlert`, and `ComputationError` are Critical ... exit 1" list):
    a failed scrub beeps, distinct from the lone Warning cause `EnospcRisk` (exit 3, no beep).
  - Add the offline-ack arm for `ScrubFailed` (mirror `SmartdAlert`) and document the
    `autoScrub`-without-`monitor` gap + the build-time warning.
- [docs/design/principles.md](../../docs/design/principles.md): review; update only if it
  enumerates alert sources (likely no change).
- User/command docs (keep in sync per AGENTS.md):
  - `docs/commands/monitor.md` -- add the scrub-failure source to the **Critical (exit 1,
    beeps)** subsection of "What triggers an alert" (it is now split into Critical vs Warning
    tiers -- *not* the Warning/ENOSPC subsection), and to the `#alert-pipeline` diagram:
    mirror the existing `smartd --start--> braid-alert.service (beeper)` /
    `--writes--> smartd-alert --> next braid monitor cycle (latches SmartdAlert)` lines with
    `braid-scrub.service --onFailure--> braid-scrub-failed.service` (starts the beeper +
    writes `scrub-failed` -> next monitor cycle latches `ScrubFailed`). Leave the exit-3
    advisory path (`EnospcRisk` -> `braid-alert-advisory.service`) untouched.
  - `docs/commands/status.md` and `docs/commands/ack.md` -- add `ScrubFailed` to the cause
    list / ack-cleanup story, including the new human banner line and the `--json` cause value
    `{"type":"scrub_failed"}` (an output-shape addition from the serde tag, no code).
  - `docs/guides/monitoring-and-alerts.md` -- user-facing alert-source list.
  - `docs/internals/tool-behavior/smartd-alerts.md` is the per-source internals analog; add
    a sibling page (or section) documenting the scrub-failure marker + flag model.
  - `README.md` if it enumerates alert causes.
- Run `scripts/docs/check-see-paths.py` / `just docs-build` (linkcheck) after ADR edits.

---

## Test plan

VM tests use a `#` preamble; Rust tests use `//`; both carry **Intent / Why it exists /
Scenario** per [testing.md](../../docs/dev/testing.md).

### Rust unit tests

- `cli/src/scrub_resume_or_start.rs` (extend `mod tests`; inject a temp marker path via
  `StatePaths::custom` so both arms run without a real `/var/lib/braid`):
  - **New** `cancelled_when_marker_present`: btrfs exit 1 + marker present ->
    `Ok(Cancelled)` (cover both the resume arm and the start-after-fallback arm).
  - **New** `failure_when_marker_absent`: btrfs exit 1 + **no** marker -> `Err`. The F2
    regression: a genuine fatal scrub error (which also sets btrfs `canceled=1`, so the
    old `Aborted`-based rule would have swallowed it) must still alert.
  - **New** `stale_marker_removed_at_entry`: a marker present before the run is cleared at
    entry, so a clean (exit 0) or genuine-failure (exit 1, no re-write) run is classified
    correctly.
  - **New** `fails_closed_when_marker_unremovable`: poison the marker path -- create a
    *directory* at `StatePaths::custom(tmp).scrub_cancel_requested()`, so `remove_file`
    returns a non-`NotFound` error on every OS (`EISDIR`/`EPERM`). Assert the command
    returns `Err(MarkerCleanupFailed)` and short-circuits **before** btrfs runs: drive it
    with a `MockRunner` that has *no* registered output, so any `runner.run` would surface
    as an unexpected-request error -- its absence proves cleanup short-circuited. Proves the
    command fails closed -- a stale/unremovable marker can never be read as `Cancelled` and
    mask a real exit 1. Regression for the fail-closed entry-cleanup policy.
  - **Update** `resume_real_failure_propagates` / `start_real_failure_propagates`: assert
    `Err` with no marker (no scrub-status probe involved).
  - Exit-3 mapping is unchanged and already covered by `resume_uncorrectable_propagates` /
    `start_uncorrectable_after_fallback`; exit 3 -> service-success is a systemd concern,
    tested in the VM (below).
- `cli/src/alert.rs` / `monitor.rs` / `status.rs` -- mirror the smartd unit tests:
  `scrub_failed_active` requires a regular file; `compute_alert_state` pushes `ScrubFailed`
  when the bool is set; `cmd_monitor` latches it from the flag; `resolve_alert_state`
  surfaces it from the flag. **Plus** the dedup regression the single-cycle smartd mirrors
  miss: `same_cause_key_scrub_failed_singleton` (mirror
  `alert.rs#same_cause_key_smartd_singleton`) **and** a two-cycle `merge_into_latch` test --
  a flag present across two monitor cycles must yield exactly **one** latched `ScrubFailed`.
  Without the `same_cause_key` arm the single-cycle push/latch tests still pass while the
  latch grows every cycle, so this two-cycle assertion is the one that catches it.
  **Plus** a **severity** pin (mirror `alert.rs`'s existing `AlertSeverity` /
  `AlertState::severity` test): `AlertCause::ScrubFailed.severity()` is
  `AlertSeverity::Critical`, and an `AlertState` whose only cause is `ScrubFailed` reports
  `Some(AlertSeverity::Critical)`. This is the F1 regression -- it fails the moment
  `ScrubFailed` is (re)classified `Warning` and silently routed to the non-beeping advisory.
  In `status.rs`, assert `format_status_human` renders the Critical **`ALERT`** banner for a
  `ScrubFailed`-only report, *not* the Warning **`NOTICE`** capacity-risk banner -- pinning the
  tier all the way to the rendered line.
- `cli/src/ack.rs` -- add the `ScrubFailed` equivalents of the smartd **snapshot-race**
  tests (per ADR 014 "Ack snapshots gating inputs"), for both mounted and offline ack:
  - flag present at the entry snapshot -> removed, `stop_beeper` fires.
  - latched `ScrubFailed` cause but flag absent at snapshot -> flag still cleared
    (crash-recovery arm, matching the smartd second-arm exception).
  - flag that arrives **after** the snapshot with no latched `ScrubFailed` -> **preserved**
    in place, left for the next monitor cycle to latch.
  - offline ack of a bare `ScrubFailed` flag -> flag removed, no `acked-stats.json` write.

### VM test -- new `tests/module/scrub-alert.{nix,py}`

Register in `flake.nix` checks:
`scrub-alert = pkgs.testers.nixosTest (import ./tests/module/scrub-alert.nix { braid = linuxCrane.braid-cli-unwrapped; });`
(module-test form, as `flake.nix` does for `braid-alert`). Two nodes; `testScript`
concatenates `dm_delay_helpers.py` like `scrub-lifecycle.nix` does. Both nodes set
`monitor.enable = true` and `monitor.alertCommand = "touch /root/alert-fired"`.

- **`fail` node -- genuine failure raises and clears the alert; clean and corruption exits
  stay silent.** Use the `lib/initrd-fixture.nix` pool (model `braid-alert.nix`). Force a
  deterministic, **exit-code-parameterized** scrub:
  `systemd.services.braid-scrub.serviceConfig.ExecStart = lib.mkForce` a small script that
  runs `exit "$(cat /run/braid-test-scrub-exit 2>/dev/null || echo 1)"` (the `mkForce`
  technique the `cancel` node uses in `scrub-lifecycle.nix`, but with the code read from a
  file). Each exit-code subtest just writes `/run/braid-test-scrub-exit` and restarts the
  service -- **no** `[Service] ExecStart=` runtime drop-in (which on a `Type=simple` unit
  would first have to clear the `mkForce`'d line with an empty `ExecStart=` before re-setting)
  and **no** `daemon-reload`. (`scrub-lifecycle.py#disable_trigger_hook` is a `[Unit]
  ConditionPathExists=` drop-in, not an `ExecStart` override -- not the pattern to copy.)
  Optionally `mkForce` `ExecStop` to a no-op so each run's `Result` reflects only the chosen
  main-process exit (no cancel-marker / `scrub-cancel` side effects on this node). Subtests
  (model `smartd-hook.py` + `braid-alert.py`):
  1. unlock pool; `systemctl start braid-scrub.service`; `wait_until ... is-failed
     braid-scrub.service`.
  2. onFailure fired: `test -f /var/lib/braid/scrub-failed`; `systemctl is-active
     braid-alert.service`; `test -f /root/alert-fired`.
  3. status names it: `braid status --json` shows a `ScrubFailed` cause and
     `alert_active: true`.
  4. monitor latches it at **Critical**: `systemctl start braid-monitor.service`; latch file
     carries `ScrubFailed`; `systemctl is-active braid-alert.service` (the Critical beeper),
     and `! systemctl is-active braid-alert-advisory.service`. The negative on the advisory is
     the load-bearing F1 assertion: it proves the monitor routed the scrub-failed latch to the
     exit-1 beeper path, not the exit-3 ENOSPC/Warning advisory.
  5. `braid ack` clears: scrub-failed flag gone, `braid-alert.service` inactive, latch
     cleared, `braid status` `alert_active: false`.
  6. **exit 3 is not an alert (corruption path).** `echo 3 > /run/braid-test-scrub-exit`;
     `systemctl start braid-scrub.service`; assert `systemctl show ... -p Result --value` ==
     `success`, `! test -f /var/lib/braid/scrub-failed`, and `braid-alert.service` not active.
     Proves `SuccessExitStatus=3` routes corruption away from `onFailure`.
  7. **exit 0 stays silent (clean monthly scrub).** `echo 0 > /run/braid-test-scrub-exit`;
     `systemctl start braid-scrub.service`; assert `Result --value` == `success`, `! test -f
     /var/lib/braid/scrub-failed`, `braid-alert.service` not active, and no
     `/root/alert-fired`. Pins the headline promise -- a normal successful scrub never beeps
     once `onFailure` is wired (previously only covered transitively).

- **`cancel` node -- lock mid-real-scrub does NOT alert (mandatory).** Model the
  `scrub-lifecycle.nix` `resume` node: dm-delay-backed **real** scrub (`setup_resume_pool`
  + `dm_delay_activate`). The preamble must state *why a real scrub is required*: the fake
  `sleep 300` path is SIGTERM-clean and would not exercise the real btrfs-exit-1-on-cancel
  + marker path. Subtests:
  1. prepare dm-delay pool, unlock, write payload, arm dm-delay.
  2. `systemctl start braid-scrub.service`; wait until `btrfs scrub status` shows
     `running`.
  3. `braid lock`.
  4. **assert `systemctl show braid-scrub.service -p Result --value` == `success`**
     (proves the marker discrimination end-to-end: ExecStop wrote the cancel-request
     marker, `scrub-resume-or-start` read it and exited 0, so the real cancel is `success`,
     not `failed`).
  5. assert `! systemctl is-active braid-alert.service`; `! test -f
     /var/lib/braid/scrub-failed`; `! test -f /root/alert-fired`.

  Suspend (`sleep.target`) is the *same* `ExecStop` cancel code path (the unit
  `Conflicts=sleep.target`), so the lock subtest is load-bearing; an explicit suspend
  subtest is optional (see `braid-auto-suspend.py` for the technique).

Optionally also add subtest 4's `Result == success` assertion to the existing
`scrub-lifecycle.py` "resume: cancel preserves Aborted state" subtest, which already
locks mid-real-scrub but never checks `Result` -- closing that blind spot in place.

---

## Verification

- `just test-rust` -- unit tests (disambiguation + alert-source mirrors).
- `just test-vm scrub-alert` -- the new two-node VM test (fail + cancel).
- `just test-vm scrub-lifecycle` -- regression: real-cancel still resumes; fake-scrub
  `cancel` node still `Result=success`.
- `just test-vm braid-alert` / `smartd-hook` -- regression on the shared alert service +
  ack.
- `just docs-build` -- mdBook linkcheck after ADR edits; `scripts/docs/check-output-ascii.py`
  for any new echo lines; `scripts/docs/check-see-paths.py` for ADR `## See` edits.
- Manual sanity (VM or NixOS host): with `monitor.enable = true`, `systemctl start
  braid-scrub-failed.service` -> beep + `braid status` shows `ScrubFailed`; `braid ack`
  silences and clears. Build a config with `autoScrub.enable = true; monitor.enable =
  false;` -> eval emits the warning, no dangling-unit error.

---

## Alternatives considered (rejected)

- **Beep-only (`onFailure -> braid-alert.service` directly, no latch).** Smaller, but a
  beep that `braid status` cannot explain contradicts ADR 014's "status is the primary
  surface." Rejected for the full latch integration.
- **`SuccessExitStatus = 1` on the scrub unit.** Exit 1 is *both* cancel and genuine
  failure, so whitelisting it would mask real failures. (Exit **3** *is* whitelisted --
  it is unambiguously "corruption found, scrub completed.") The cancel-vs-failure split
  must live in braid, via the cancel-request marker. Rejected.
- **Disambiguate cancel via `btrfs scrub status` (`Aborted`/`canceled=1`).** Rejected:
  `scrub.c#scrub_one_dev` sets `canceled = !!ret` for *any* nonzero scrub ioctl, so a
  fatal scrub error also renders as `aborted` -- the status flag cannot prove a deliberate
  cancel and would swallow real failures (reviewer F2). The explicit cancel-request marker
  is authoritative.
- **Re-map exit 3 to failure (alert on corruption via the scrub unit).** Explicitly
  forbidden by [ADR 014](../../docs/design/decisions/014-alerts.md): corruption already
  alerts via the device-stats poll; a scrub-status probe "would be redundant." Rejected --
  hence `SuccessExitStatus=3`, which keeps exit 3 off the `onFailure` path.
- **Standalone `braid-alert.service` (decoupled from `monitor.enable`).** Would beep on
  scrub *failure* without the monitor but stay silent on scrub-*found corruption* (which
  needs the device-stats poll) and offer no status/ack integration -- incoherent partial
  coverage for more code. Rejected in favor of gate + warning.
- **Fold scrub-failure detection into `braid monitor` (poll `systemctl is-failed`
  instead of `onFailure`).** The `failed` state is transient (a resume/timer re-run
  clears it), so a 5-min poll can miss it; the flag file is durable. `onFailure` + flag
  is the robust shape. Rejected.

## Implementation notes

- `cmd_scrub_resume_or_start` gained a `paths: &StatePaths` parameter (for the
  injected marker path); `main.rs#Commands::ScrubResumeOrStart` passes the
  already-in-scope production `paths`. The marker-discrimination logic is factored
  into two helpers -- `clear_stale_cancel_marker` (fail-closed entry remove) and
  `classify_btrfs_failure` (shared by both the resume and start arms).
- The cancel-marker unit tests model `ExecStop` via a `MockRunner::with_handler`
  that writes the marker as a *side effect of the btrfs call* returning exit 1.
  Writing it before the call would be cleared by the entry-remove, so this is the
  only faithful way to put the marker "in flight for this run" against a
  synchronous mock.
- `ack_offline` reached 8 parameters (over clippy's 7 threshold) once the
  `scrub_failed_active` + `remove_scrub_failed` params were added, so it carries
  `#[allow(clippy::too_many_arguments)]` -- the same idiom already used twice in
  `recover.rs`.
- The `options.nix` warning predicate is `cfg.autoScrub.enable && !cfg.monitor.enable`
  (the plan wrote `cfg.enable && ...`); the `cfg.enable &&` is dropped because the
  whole `config` block is already `lib.mkIf cfg.enable`, matching the sibling
  `fan-control.nix` warning idiom.
- The internals page is a standalone sibling, `docs/internals/tool-behavior/scrub-failure-alerts.md`
  (registered in `SUMMARY.md`), rather than a section inside `smartd-alerts.md`.
- A few alert.rs unit tests beyond the plan's minimal list were added to mirror the
  EnospcRisk set: `merge_carries_forward_latched_scrub_failed` and
  `scrub_failed_latch_roundtrip_and_json_shape` (serde-shape pin), plus separate
  resume-arm and start-arm cancel tests.

## Follow Up

- The new `scrub-alert` VM test and the modified `scrub-lifecycle.py` assertion were
  validated only by `nix eval` of the flake check (derivation builds, module + script
  compose); the behavioral VM run (`just test-vm scrub-alert`, `just test-vm scrub-lifecycle`)
  was not executed here because it needs the aarch64-linux builder. Run both on CI / a
  builder to confirm the end-to-end onFailure -> alert -> ack flow and the real-cancel
  `Result=success` path.
