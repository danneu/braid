# Centralize Missing-Device Replace Guidance

## Summary

Refactor all missing-device repair guidance to share one crate-internal command-shape helper. Runtime messages must no longer suggest bare `braid replace --missing-id <devid>`; they should show the runnable required shape:

`braid replace --old <missing-name> --new <new-name>=/dev/disk/by-id/<...>`

and mention `--missing-id <devid>` only as an optional cross-check, or include it after the required args when the message has an actual devid.

## Implementation Changes

- Add a small `pub(crate)` helper module, e.g. `cli/src/repair_hint.rs`, exported from `lib.rs` with doc comments per repo rules.
- Provide shared helpers for:
  - base missing-replace command with placeholders
  - base command plus placeholder `--missing-id <devid>`
  - base command plus actual `--missing-id {devid}`
  - one short optional-cross-check phrase
- Update runtime operator-facing hints in `add`, `preflight`, `replace`, `pool`, `doctor`, and `remove_missing` to use the helper instead of inline command fragments.
- Keep each caller's local context, pluralization, and remediation ordering; only centralize the replace command shape and cross-check wording.
- For `doctor` multi-missing output, render the listed devids once, one base replace command, and a separate optional-cross-check phrase telling the operator to use one of the listed IDs with `--missing-id <devid>`.
- Update docs that currently propagate the shorthand: the two ADRs, command docs that discuss missing replacement, and `docs/guides/recovery-scenarios.md`. In the recovery guide, rewrite the dead-disk paragraph so `--missing-id` is an optional cross-check using the full command shape, not something replace may require.
- Leave flag-reference wording alone where it is clearly describing `--missing-id` as a flag, not a command invocation.
- Do not change clap args, replace behavior, degraded-pool policy, or generated `docs/book/`.

## Test Plan

- Add unit tests for the new helper strings, including the actual-devid variant.
- Update existing string assertions to check the full required shape, especially add warning rendering, preflight degraded rejection, replace mixed-state rejection, pool remove hint, doctor missing-device check, and remove-missing 2-disk rejection.
- Add a doctor unit test with two missing devids to pin plural rendering, the single base replace command, and the optional `--missing-id <devid>` cross-check phrase.
- Update VM text pins in `tests/cli/braid-add-warnings.py`, `tests/cli/replace-live-disk.py`, and `tests/cli/remove-missing-2disk-rejected.py`.
- Run:
  - `just test-rust`
  - `just test-vm braid-add-warnings replace-live-disk remove-missing-2disk-rejected`
  - `just check-docs`
- Verify with `rg` that no operator-facing runtime string still contains bare ``braid replace --missing-id <devid>``. Remaining matches should be comments/tests about the flag itself or explicit full command shapes.

## Assumptions

- `--missing-id` remains optional and is only a cross-check; `--old` identifies the member being replaced.
- The preferred placeholder shape is `<missing-name>` and `<new-name>=/dev/disk/by-id/<...>`.
- This is a wording/refactor change only; no CLI semantics or recovery behavior changes.
- Preserve unrelated dirty worktree changes and do not run formatters.
