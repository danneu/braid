# Document all public command entry points + guard the convention

## Context

braid's doc-comment convention (`docs/dev/doc-comments.md`, AGENTS.md "Doc
comments" bullet) requires every `pub`/`pub(crate)` Rust CLI item to carry a
`///` stating *why it exists at its boundary* -- intent, invariant, ownership,
or call-site coupling, not the signature.

A review finding flagged `cmd_idle` (`cli/src/idle.rs#cmd_idle`) for missing this
`///`. Investigation showed it is not a one-off: **12 of the 20 public
`pub fn cmd_*` command entry points lack the required doc**, including the
heaviest, safety-critical ones (`cmd_lock`, `cmd_unlock`, `cmd_remove`,
`cmd_add`, `cmd_replace`, `cmd_recover`, `cmd_doctor`). The root cause is that
the convention is **review-only** -- nothing in CI checks it, while braid guards
its *other* conventions (ASCII output, See-paths, doc-anchors, plan-refs) with
small `scripts/docs/check-*.py` checkers. So the convention silently drifted.

Patching `cmd_idle` alone would leave 11 siblings to generate the identical
finding and would not stop the 21st command from drifting again. The ideal fix
dissolves the class: **document all 12 entry points, and add a narrow CI guard**
so the gap cannot recur.

Outcome: every public command entry point carries a boundary `///`, and CI fails
any future `pub fn cmd_*` that ships without one.

## Scope

- **In:** the 12 undocumented `pub fn cmd_*` entry points + one enforcement
  checker following the existing `scripts/docs/check-*.py` house pattern.
- **Out:** the broader convention gap. There are hundreds of other undocumented
  `pub` items in `cli/src`, but most are struct fields / enum variants that the
  convention's own skip list and "do not write recoverable-from-code comments"
  rule exclude. The `cmd_*` entry points are the clean, high-value, user-facing
  slice. A wider sweep is a separate initiative; this plan does not block it
  (the checker can later widen its match set).

## Part 1 -- Enforcement guard (do this first, TDD-style)

New checker `scripts/docs/check-cmd-doc-comments.py`, modeled on
`scripts/docs/check-see-paths.py` (structure) and
`scripts/docs/check-output-ascii.py` (`--selftest` convention, tracked-file
scan over `cli/src/**/*.rs`).

Behavior:
- Scan tracked `cli/src/**/*.rs`.
- Find every `pub fn cmd_*` (also accept `pub(crate) fn cmd_*` for safety).
- For each, walk upward skipping blank lines and contiguous `#[...]` attribute
  lines; the nearest preceding non-blank/non-attribute line must start with `///`.
  The convention is specifically `///` (`docs/dev/doc-comments.md`), so a `/**`
  block doc does NOT satisfy the guard even though Rust would treat it as a doc.
  Skipping blank lines mirrors Rust doc association (a `///` attaches across blank
  lines + attributes -- verified: `///` + blank line + `pub fn` compiles clean
  under `deny(missing_docs)`/`deny(unused_doc_comments)`), so the guard never
  false-positives on a `///` the compiler already accepts.
- On any miss: print `path:line: <name> lacks a /// boundary doc comment` to
  stderr, return 1. On success: print an ok line to stdout, return 0.
- `--selftest`: assert the checker flags undocumented `pub fn cmd_x`, undocumented
  `pub(crate) fn cmd_x` (so the visibility match cannot silently regress to
  `pub`-only and leave future `pub(crate)` command helpers unguarded), AND a
  `pub fn cmd_x` whose only doc is a `/**` block comment (locking in the
  `///`-only contract so the guard cannot drift back to accepting block docs);
  and passes the documented forms -- including a `///` separated from the fn by a
  blank line and by an `#[inline]` attribute -- so blank lines, attributes, and
  `pub(crate)` never cause a false result in either direction.

Accepted limitation (matches `check-output-ascii.py`): multi-line `#[...]`
attributes are classified from their first line; the `cmd_*` set uses
single-line attributes exclusively today.

Wiring (mirror the existing checks exactly):
- **`justfile`**: add a `check-cmd-doc-comments` recipe (with explanatory
  comment) that runs `--selftest` then the full check -- same two-line shape as
  `check-output-ascii` / `check-code-doc-anchors` (`justfile` ~lines 326-348).
- **`.github/workflows/checks.yml`**: add a `cmd-doc-comments` job mirroring the
  `output-ascii` job (`checkout` -> selftest -> full check).

TDD ordering: land the checker first and run it -- it must go **red**, naming all
12 functions below. That proves the guard works before Part 2 turns it green.

## Part 2 -- The 12 boundary doc comments

Each draft below is grounded in the function body / its plan-execute split and
calibrated to the house voice (`cmd_lock_systemd_stop`, `cmd_lock_orchestrate`,
`IdleResult`/`BusyReason`). Plain ASCII (`->`, `--`). **Treat each as a starting
point: confirm the contract against the function before finalizing** -- drafts
for the 11 non-idle functions came from a code survey, not a full read.

Common shape (most mutating commands): a dry-run/preview vs execute boundary per
[ADR 022](docs/design/decisions/022-dry-run-preview-model.md) -- planning does
all probing/preflight and accumulates notes; `params.dry_run` gates
`plan.preview()` vs `plan.execute()`; on failure the accumulated notes render to
stderr via the same helper as the Ok path. Say what is *specific* to each.

| Function | File | Proposed `///` |
|---|---|---|
| `cmd_idle` | `cli/src/idle.rs` | Autosuspend gate: probes mount -> host-wide sysfs exclop -> pool-scoped scrub, in that order, and maps every unknowable probe to `Busy`, never `Idle`. |
| `cmd_remove` | `cli/src/remove.rs` | Plan-then-execute device removal; `dry_run` gates preview vs execute. The relocation-space preflight is asymmetric: soft-warn-and-proceed on >=2-survivor uncertainty, but fail-closed on the single-survivor (2->1) capacity branch (every input uncertainty -> refuse). Losing RAID1 redundancy is a confirmation-only warning, not a refusal. (See `check_eviction_space` / `EvictionCheck` docstrings.) |
| `cmd_remove_missing` | `cli/src/remove_missing.rs` | Plan-then-execute removal of missing devices with fail-closed relocation-space preflight. Unlike `cmd_remove` this is always a degraded-array context, so it has no soft-warn branch. |
| `cmd_add` | `cli/src/add.rs` | Plan-then-execute device enrollment (LUKS format + btrfs add); `dry_run` gates preview vs execute, with fail-closed duplicate-UUID/name preflight. |
| `cmd_unlock` | `cli/src/unlock.rs` | Plan-then-execute LUKS open + pool mount; `dry_run` gates preview vs execute. Probe notes render before any mutation; credential resolution is skipped when every mapper is already open. |
| `cmd_lock` | `cli/src/lock.rs` | User-initiated lock entry point (systemd ExecStop uses `cmd_lock_systemd_stop`). Runs the shared lock impl with `LockMode::User` preflight, which gates on exclusive ops -- unlike the shutdown path, it will not lock during a balance. |
| `cmd_replace` | `cli/src/replace.rs` | Plan-then-execute device replacement; `dry_run` gates preview vs execute. Planning does membership load, mount/preflight, duplicate-UUID check, and new-target probe; execute verifies the passphrase and holds a sleep inhibitor. |
| `cmd_recover` | `cli/src/recover.rs` | Plan-then-execute recovery from an interrupted operation; `dry_run` gates preview vs execute. Planning loads the journal, resolves admission membership, and plans the mount/open sequence; execute verifies credentials and replays journal actions. |
| `cmd_enroll_key_file` | `cli/src/enroll_key_file.rs` | Plan-then-execute keyfile enrollment; `dry_run` gates preview vs execute. Planning validates the keyfile path and discovers candidates without reading the passphrase. The dry-run preview does passphrase-free slot/keyfile classification; the real run leaves classification to execute, which reads the passphrase, re-probes each candidate, then classifies and enrolls. |
| `cmd_monitor` | `cli/src/monitor.rs` | Headless monitor cycle: probe pool, diff device stats against the acked baseline, latch alerts, return merged state. Fail-closed on probe uncertainty (unknown -> alert) per ADR 014; ENOSPC risk is the fail-open carve-out. |
| `cmd_status` | `cli/src/status.rs` | Read-only pool status (JSON or human): renders the persisted alert latch plus live mounted-pool topology and capacity. Never quarantines a corrupt latch -- it fails loud and leaves repair to the monitor. |
| `cmd_doctor` | `cli/src/doctor.rs` | Non-mutating diagnostic entry point: wires the real runner/filesystem into `run_doctor`, prints human or JSON, and returns the overall-status exit code (Fail -> error; Warn/Ok/Skip succeed). |

## Part 3 -- Convention doc note

Add one line to [`docs/dev/doc-comments.md`](docs/dev/doc-comments.md) noting
that the `pub fn cmd_*` subset is now CI-enforced by
`scripts/docs/check-cmd-doc-comments.py`, so contributors know the guard exists
and where to look when it fails.

## Files to modify

- `scripts/docs/check-cmd-doc-comments.py` (new)
- `justfile` (new `check-cmd-doc-comments` recipe)
- `.github/workflows/checks.yml` (new `cmd-doc-comments` job)
- `docs/dev/doc-comments.md` (one-line enforcement note)
- The 12 command files listed in the table (add one `///` each)

## Verification

1. **Guard goes red first:** after Part 1, `python3
   scripts/docs/check-cmd-doc-comments.py` exits 1 and names all 12 functions;
   `python3 scripts/docs/check-cmd-doc-comments.py --selftest` exits 0.
2. **Guard goes green:** after Part 2, `just check-cmd-doc-comments` (selftest +
   full) exits 0.
3. **No regressions in sibling checks:** `just check-output-ascii` still passes
   (plain `///` is comment-context and exempt; keep the new docs ASCII anyway).
4. **Compiles clean:** `cargo build` and `cargo clippy --all-targets` pass (doc
   comments are inert; clippy must not flag empty/dangling doc lines).
5. **Docs build clean:** `cargo doc --no-deps` builds with no warnings,
   confirming each new `///` is well-formed and attached (catches an
   `unused_doc_comments` / empty-doc mistake). Note: a blank line between `///`
   and the fn does NOT detach the doc, so that is not a failure mode to guard.

## Follow Up

- Fix the existing rustdoc `invalid_html_tags` warning in `cli/src/main.rs#LuksFormatArgs` by escaping or backticking `--luks-format-arg=<ARG>`.
