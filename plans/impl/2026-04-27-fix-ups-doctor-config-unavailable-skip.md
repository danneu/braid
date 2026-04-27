# Fix UPS doctor skip reason when config is unavailable

## Summary

`run_doctor` calls `check_config_schema` before the UPS checks, and
`check_config_schema` only populates `ctx.config` after full config
deserialization succeeds. If the config file is valid JSON but fails schema
validation, the UPS checks run with `ctx.config = None`.

Fix the misleading output by distinguishing these cases:

- `ctx.config == None` -> `Skip`, message `skipped (config not available)`
- config loaded but `ups` is absent or `ups.enable = false` -> `Skip`, message
  `skipped (braid.ups not enabled)`

Do not change check ordering, severity, or any UPS command behavior.

## Key changes

- In `check_ups_daemon_up`, check `ctx.config.as_ref()` first.
  - If missing, return `Skip` with `skipped (config not available)`.
  - If present, keep the current `config.ups()` handling and existing `upsc`
    probe logic unchanged.

- In `check_braid_online_active_when_mounted`, check `ctx.config.as_ref()`
  first.
  - If missing, return `Skip` with `skipped (config not available)`.
  - If present, check `config.ups().is_some_and(|u| u.enable)`.
  - Keep the existing mountpoint check and
    `systemctl is-active braid-online.service` behavior unchanged.
  - Remove the later redundant `None` branch for `mount_point`, since config has
    already been proven present.

- Update the two function comments so they mention config-unavailable skip
  behavior.

## Test plan

- Add a regression unit test for `run_doctor` with valid JSON that contains
  `ups.enable = true` but fails schema validation, for example an empty
  `mount_point`.
  - Assert `config_file` is `Ok`.
  - Assert `config_schema` is `Fail`.
  - Assert `ups_daemon` is `Skip` with `config not available`.
  - Assert `braid_online_active` is `Skip` with `config not available`.

- Keep existing tests for absent UPS and `ups.enable = false`; they should still
  expect `skipped (braid.ups not enabled)`.

- Run:

  ```sh
  cargo test doctor::tests
  just test-rust
  ```

## Assumptions

- The fix should not attempt partial parsing from `ctx.config_value`; a
  schema-invalid config should not be treated as authoritative for UPS state.
- This is a diagnostic clarity fix only. It should not upgrade the
  unavailable-config UPS checks from `Skip` to `Warn` or `Fail`, because
  `config_schema` already reports the hard failure.
