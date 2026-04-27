# Update `remove-missing` no-missing validation wording

## Context

`plan_remove_missing` currently has two adjacent validation paths for
missing-device targeting:

1. `pool.missing_count == 0` returns
   `no missing devices detected in pool.`
2. `!pool.missing_devids.contains(&params.missing_id)` returns
   `devid X is not a device in this pool...`

The zero-missing branch is not safe to collapse because it also preserves
error precedence. In a healthy pool, if the operator passes a live devid
as `--missing-id`, current behavior reports "no missing devices" before
the live-device validation can fire. Keep that branch explicit and only
update its wording to include the requested devid.

One detail is important: choose the no-missing wording from
`pool.missing_count == 0`, not from `pool.missing_devids.is_empty()`.
`probe_pool` can report `missing_count > 0` while `missing_devids` is
empty for null-underlying hot-unplugged devices; those must not be
described as "no missing devices".

## Change

### `cli/src/remove_missing.rs`

1. Keep the standalone `if pool.missing_count == 0` branch before the
   live-device validation, and update only its message:

   ```rust
   if pool.missing_count == 0 {
       return RemoveMissingPlanReport {
           notes: std::mem::take(&mut notes),
           result: Err(RemoveMissingError::Validation(format!(
               "no missing devices detected in pool (devid {} was not found among them).",
               params.missing_id
           ))),
       };
   }
   ```

2. Leave the live-device validation after the zero-missing branch:

   ```rust
   if pool.devices.iter().any(|d| d.devid == params.missing_id) {
       return RemoveMissingPlanReport {
           notes: std::mem::take(&mut notes),
           result: Err(RemoveMissingError::Validation(format!(
               "devid {} is a live device, not a missing one. \
                Use 'braid remove' to remove live devices.",
               params.missing_id
           ))),
       };
   }
   ```

3. Leave the missing-devid membership validation as the generic wrong-ID
   branch:

   ```rust
   if !pool.missing_devids.contains(&params.missing_id) {
       return RemoveMissingPlanReport {
           notes: std::mem::take(&mut notes),
           result: Err(RemoveMissingError::Validation(format!(
               "devid {} is not a device in this pool. \
                Use 'braid status' to see device IDs.",
               params.missing_id
           ))),
       };
   }
   ```

4. Do not change command flags, mutation order, journal behavior, pool
   probing, or null-underlying handling.

## Tests

Update the existing unit coverage in `cli/src/remove_missing.rs`:

1. Keep `plan_remove_missing_rejects_wrong_missing_id_from_pool_state`
   passing for a pool that has one real missing devid and the operator
   passes `--missing-id 99`. The expected message remains:

   ```text
   devid 99 is not a device in this pool. Use 'braid status' to see device IDs.
   ```

2. Update `plan_remove_missing_preserves_preflight_notes_on_no_missing_devices`
   so it asserts the combined no-missing message contains both:

   ```text
   no missing devices detected
   devid 999
   ```

   Keep its note-preservation assertions unchanged.

3. Add a focused unit test for zero-missing precedence:
   healthy pool, `--missing-id` equal to a live device devid, and the
   result should still use the no-missing message instead of the
   live-device message.

4. Add a focused null-underlying validation test:
   make `probe_pool` yield `missing_count = 1` and `missing_devids = []`
   by returning `btrfs filesystem show` output with two mapper paths and
   `Total devices 2`, then returning `cryptsetup status` with `device:
   (null)` for one mapper. Pass that null-underlying devid as
   `--missing-id` and assert the error does not contain
   `no missing devices detected`; assert it uses the generic
   `devid X is not a device in this pool...` message.

## Verification

Run:

```sh
just test-rust
```

Expected outcome:

- `remove_missing` unit tests pass.
- The no-missing path preserves accumulated preflight notes.
- Wrong missing IDs in degraded pools still get the existing
  `braid status` hint.
- Zero-missing pools still report "no missing devices detected" even
  when the requested devid belongs to a live device.
- Null-underlying pools with `missing_count > 0` and empty
  `missing_devids` do not report "no missing devices detected".

## Assumptions

- This is a zero-missing validation wording improvement; it should not
  make any new `remove-missing` input valid.
- No README or decision-doc update is needed because only one validation
  error string changes, and that string is covered by unit tests.
