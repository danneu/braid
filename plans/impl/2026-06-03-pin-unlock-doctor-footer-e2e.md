# Pin the doctor-recovery footer end-to-end on the unlock degraded-refusal path

## Context

A degraded-refusal from `braid unlock` (or `braid recover`) appends a
`run 'braid doctor' for recovery guidance` footer **only** when a missing member
has an unreadable LUKS header (`format_degraded_refused`, `cli/src/mount.rs#format_degraded_refused`,
gate at `mount.rs:83-85`). That footer is the only bridge from the terse
degraded-refusal lines to the full recovery flow for a bricked header.

It is pinned today only at the pure-function layer
(`format_degraded_refused_unreadable_includes_doctor_footer`, `mount.rs:1526`).
The VM test that exercises the real pipeline -- `tests/cli/braid-unlock.py`
Test 7 -- already drives an unreadable member end-to-end and asserts the header
line, the `raw: LUKS header unreadable` per-disk line, and the `--allow-degraded`
hint, but it never asserts the footer.

The gap is a *composition* gap, not a wiring gap. `print_cli_error` is a single
`eprintln!` of the whole joined message (`main.rs:1282-1288`), so the footer
cannot vanish "in the wiring" while the earlier lines survive -- any truncation
there would also drop the per-disk and `--allow-degraded` lines Test 7 already
asserts (`braid-unlock.py:538-541`). And the footer gate in isolation is already
covered by three pure-function tests (unreadable -> footer at `mount.rs:1526`,
unplugged -> no footer at `:1546`, mixed -> at-most-once at `:1555`). What no test
currently pins is the *composition* with the live probe: that the real
`plan_open_pool` classification which renders the `raw: LUKS header unreadable`
per-disk line also trips the `is_luks_header_state()` footer gate in the same
emitted message. The E2E assertion guards against the per-disk reason text and the
footer-gate predicate diverging in future -- e.g. a new `MissingReason` variant
that reuses the "LUKS header unreadable" per-disk text but is not in
`is_luks_header_state()`, or relocation of the footer out of the pure function
(where the unit test would no longer pin its presence in real output). This change
closes that gap with one behavioral assertion in the subtest that already builds
the scenario.

Scope decision: **only** the unlock-path assertion. Three siblings are
deliberately excluded:

- **`braid recover`** needs no parallel pin. Its degraded-refusal render arm
  (`main.rs:1009-1012`) is byte-identical to unlock's
  (`print_cli_error(&msg); std::process::exit(2);`) and routes the same
  `format_degraded_refused` output -- both commands reach it through
  `mount::plan_open_pool` -- so the unlock E2E assertion is representative. (The
  existing recover degraded-refusal VM test, `braid-recover.py` Test 3a, drives an
  *unplugged* disk, which correctly emits no footer, so it neither covers nor needs
  the unreadable-header footer.)
- **`braid status`** footer (`status.rs:1482`) is out of scope: its gate is a
  trivial `matches!(d.status, DiskStatus::LuksHeaderUnreadable)` (`status.rs:1462`)
  on an already-classified status -- no probe wiring between classification and
  footer -- so the renderer is exercised by the unit test
  `status_verbose_luks_header_unreadable_disk` (`status.rs:2821`). That test pins
  only the `braid doctor` substring (`status.rs:2863`), not the full
  `run 'braid doctor' for recovery guidance` line, so the status literal is *not*
  byte-pinned; that is acceptable here, since the status path's underlying
  header-unreadable probe is already covered end-to-end by the doctor VM tests and
  closing the status footer's own composition gap is a separate, lower-value task.
- **A shared const** for the footer string is out of scope: the unlock
  degraded-refusal footer and the status `Action:` line are *independent surfaces*
  with no byte-identity contract. They share wording today, but nothing requires
  them to stay identical -- so there is no drift *bug* to protect against, only an
  allowed divergence. Hoisting the literal into a shared const would manufacture a
  coupling that is not a real invariant, and the new boundary could not honestly
  satisfy braid's `///` doc-comment rule.

## Change

Single file: `tests/cli/braid-unlock.py`, Test 7 subtest (the
`with subtest("Test 7: uninitialized disk detected ...")` block, currently
`braid-unlock.py:505-563`).

Test 7 already captures combined stdout+stderr into `output` (it runs
`unlock_cmd(passphrase) + " 2>&1"` and reads `ret[1]`), so the footer -- emitted
to stderr via `print_cli_error` -> `eprintln!` -- is already present in `output`.
The assertion will pass immediately; it is a pin, not a behavior change.

Add one positive assertion immediately after the existing `--allow-degraded` hint
assertion (`braid-unlock.py:540-541`), with a short inline comment matching the
file's existing commenting density (cf. the "Cross-command negative invariant"
comment block already in Test 7). The comment must state the invariant and why the
unit test is insufficient. Shape:

```python
    # Composition pin: the live plan_open_pool probe must classify the raw member
    # so that format_degraded_refused renders BOTH the "raw: LUKS header unreadable"
    # line AND the doctor footer in the same message. The pure-function tests cover
    # the footer gate in isolation; this is the only check that the live per-disk
    # reason text and the is_luks_header_state() footer gate stay in agreement (a
    # future MissingReason variant reusing the per-disk text but not the gate, or
    # the footer moved out of the pure function, would slip past every other test).
    assert "run 'braid doctor' for recovery guidance" in output, \
        f"Expected doctor-recovery footer on unreadable-header degraded-refusal, got: {output}"
```

Use straight ASCII (`->`, `--`) per the project's CLI/comment style; no em-dashes
or fancy Unicode.

No production code, no Rust tests, no docs, and no other test files change.

## Files

- `tests/cli/braid-unlock.py` -- add one assertion + comment inside the existing
  Test 7 subtest. (Only file modified.)

Reference points (read-only, no change):
- `cli/src/mount.rs#format_degraded_refused` -- footer gate (`mount.rs:83-85`).
- `cli/src/main.rs` -- `DegradedRefused` arms (`:738-741`, `:1009-1012`) ->
  `print_cli_error` (`:1282-1288`), the verbatim render path.
- `cli/src/mount.rs:1526` -- the pure-function test this VM assertion complements.

## Verification

1. `just test-vm braid-unlock` -- runs the `braid-unlock` NixOS VM check
   (`flake.nix:547`). Test 7's new assertion must pass; all other subtests
   (including the dry-run/real-run degraded-refusal contract in Test 4a/4a_dry and
   the `--allow-degraded` mount in Test 4b) must stay green.
2. Sanity-check the pin actually bites: transiently delete the footer line in
   `format_degraded_refused` (`mount.rs:84`) locally, confirm `just test-vm
   braid-unlock` now fails on the new assertion (and on the existing unit test),
   then revert. This proves the assertion is a real tripwire, not a tautology.
   (Optional manual confidence step; do not commit the transient deletion.)

This is a localized test-only change; no broad-blast-radius suite run is required.
Hand back to the user for any full-suite rerun.
