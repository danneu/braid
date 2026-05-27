# Render the status missing-device Action line as a runnable replace hint

## Context

`braid status` prints an `Action:` line for a missing (or erroring,
named) member row. At HEAD that line is built in
`cli/src/status.rs:1413-1419` as:

```
    Action:  add replacement disk to config, then: braid replace --old <name> --new <new-name>=/dev/disk/by-id/<...>
```

The `add replacement disk to config, then:` prefix is misleading.
`braid replace` owns pool membership itself: it resolves `--old`
against `pool.json`, takes the replacement inline via
`--new <name>=<by-id-path>`, and journals the membership transition
(`cli/src/replace.rs` computes and persists `pre_membership` ->
`target_membership`; `docs/commands/replace.md` shows the single
copy-paste invocation). There is no separate "add the disk to config"
step for the operator to do first. The prefix therefore instructs work
that does not exist and buries the one command the operator should run.

Goal: the missing-device `Action:` line presents the shared repair hint
directly, so it is copy-paste runnable.

## Change

`cli/src/status.rs`, the `Some(name)` arm of the missing/erroring member
Action branch (~`status.rs:1416-1419`):

- Keep `let repair_command = repair_hint::missing_replace_command(Some(name));`.
- Drop the `add replacement disk to config, then: ` prefix so the line
  renders as just the hint:

  ```rust
  out.push_str(&format!("    Action:  {repair_command}\n"));
  ```

  i.e. `    Action:  braid replace --old disk1 --new <new-name>=/dev/disk/by-id/<...>`.

This is the only site that emits the prefix --
`git grep "add replacement disk to config" -- cli/src` returns only this
line. Reuse
the existing `repair_hint::missing_replace_command` helper; no new module,
no signature change, no other branch or callsite touched.

## Tests

Both Rust unit tests that render a named missing/erroring member Action
line already assert the positive shape
(`human.contains("braid replace --old disk1 --new <new-name>=/dev/disk/by-id/<...>")`)
and a sibling negative (`!human.contains("replace --missing-id")`). Add one
more negative assertion beside each, locking in the prefix removal:

- `cli/src/status.rs` `build_disk_reports_routes_foreign_mapper_errors_to_doctor`
  (~`status.rs:5267`): add `assert!(!human.contains("add replacement disk to config"), ...)`.
- `cli/src/status.rs` `build_disk_reports_missing_member_keeps_replace_action_target`
  (~`status.rs:5334`): add the same.

The assertion is behavioral and structure-insensitive: it pins
user-facing wording (the misleading prefix is gone), not internal layout.

## Verification

- `just test-rust` -- exercises the `format_status_human` rendering path
  end-to-end through the two updated status tests, plus the full Rust
  suite.
- Sanity: `git grep -n "add replacement disk to config" -- cli/src` --
  the only remaining hits are the two new negative test assertions in
  `status.rs`; no production rendering path contains it. The `-- cli/src`
  scope is load-bearing: the phrase also lives in committed `plans/impl/`
  history, so an unscoped grep returns those too and the check stops being
  a clean pass/fail.

## Out of scope / non-goals

- No changes to the `repair_hint` module, its callers in
  `add`/`doctor`/`pool`/`preflight`/`remove`/`remove_missing`/`replace`,
  or any `--missing-id` behavior -- only the status Action prefix.
- No doc edits; no doc references the prefix.
- No formatter runs; narrow hand edit only (per AGENTS.md).
