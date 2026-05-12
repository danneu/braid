# UPS JSON Empty-Status Warning

## Summary

Add an explicit JSON warning sentinel for `braid ups status --json` when
`upsc` succeeds but `ups.status` is empty or missing. Keep the command
behavior-preserving: exit code stays 0, the parsed `UpscOutput` fields
remain top-level, and scripts get a clear contract that `.error` or
`.warning` means the typed body is not trusted healthy state.

## Key Changes

- Replace the success arm of `JsonReport` with a flattened success wrapper.
- Healthy output remains the same shape as today, with top-level
  `status_flags`, `battery`, `input`, `device`, `extra`, etc.
- Empty-status output adds top-level `"warning": "ups_status_empty"`
  alongside those existing fields.
- Do not emit `"warning": null` on healthy output.
- Add a small private `JsonWarning` enum with `ups_status_empty`, and a
  success constructor/helper that sets the warning when
  `parsed.status_flags.is_empty()`.
- Leave these behaviors unchanged:
  - `parse_upsc` still returns an empty flag set for absent or empty
    `ups.status`.
  - Human output still prints `Status: (unknown -- ups.status missing)`.
  - Preflight still refuses empty status.
  - Doctor still warns.
  - Query and invocation failures still emit `"error"` sentinels and exit 1.
  - UPS-not-enabled still emits `"error": "ups_not_enabled"` and exits 0.

## Documentation

- Update `manual/commands/ups-status.md`.
- Define trusted success as reachable UPS with populated `status_flags` and
  no `.error` or `.warning`.
- Add the empty-status warning row: JSON contains
  `"warning": "ups_status_empty"` plus the flattened parsed body, exit code 0.
- State the scripting rule explicitly: if `.error` or `.warning` is present,
  do not treat the body as healthy UPS state.
- Update `docs/decisions/020-ups-integration.md` to note that `--json`
  preserves the typed parsed model but may add a top-level warning sentinel
  for empty `ups.status`.

## Test Plan

- Add Rust unit coverage in `cli/src/ups.rs`.
- Healthy `JsonReport` has `status_flags` and no `warning`.
- Empty `status_flags` serializes with `"warning": "ups_status_empty"` and
  still includes the parsed body.
- Existing error sentinel tests remain unchanged.
- Add a `check_ups_daemon_up` unit test in `cli/src/doctor.rs` where `upsc`
  exits 0 with telemetry but no `ups.status`; assert `CheckStatus::Warn` and
  a message mentioning empty `ups.status`.
- Extend `tests/cli/braid-status-ups.nix` with a second dummy UPS, e.g.
  `emptyups`, whose `.dev` file has telemetry and an explicit empty
  `ups.status:` line. The `dummy-ups` driver initializes a missing
  `ups.status` to `OL`, so the fixture must use the empty-line shape.
- Extend `tests/cli/braid-status-ups.py` before stopping `upsd.service`:
  - Wait for the empty-status driver to publish a seeded non-status key with
    `machine.wait_until_succeeds("upsc emptyups@localhost battery.charge", timeout=60)`.
  - Create a temp config that points `.ups.name` to `emptyups`.
  - Run `braid --config /tmp/empty-ups.json ups status --json`.
  - Assert exit 0, `warning == "ups_status_empty"`, `status_flags == []`,
    useful telemetry still parses, and no `error` field is present.
- Verification commands:
  - `just test-rust`
  - `just test-vm braid-status-ups`

## Assumptions

- The best fix is the behavior-preserving warning sentinel, not converting
  empty status into a non-zero `query_failed` error.
- The warning field is top-level and flattened with the existing success body,
  so existing scripts that read `.status_flags` do not need to switch to a
  nested `.body`.
- The sentinel string is exactly `ups_status_empty`.
