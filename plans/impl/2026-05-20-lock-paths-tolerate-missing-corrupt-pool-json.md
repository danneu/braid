# Plan: lock paths tolerate missing/corrupt pool.json

## Context

`run_systemd_stop_lock` (`cli/src/main.rs:1016-1068`) is the ExecStop reentry
for `braid-online.service`. It is the only documented path that cleans up
orphan `/dev/mapper/braid-*` mappers when `braid-online.service` is stale --
see `tests/module/execstop-cleans-stale-online.py` and
`docs/decisions/018-systemd-lifecycle.md:139-142`.

That path currently calls `load_membership_or_exit(paths, 1)` at
`cli/src/main.rs:1061`, which `print_cli_error` + `std::process::exit(1)` on
a missing or corrupt `pool.json`. Recovery scenarios where `pool.json` is
absent, moved aside, or unparseable (the workflow documented in
`docs/luks-unlock.md:143-170` and `docs/decisions/017-runtime-disk-membership.md:62-66`)
can therefore leave the ExecStop unable to complete:

- At shutdown, systemd marks ExecStop failed, leaks open LUKS mappers, and
  may SIGKILL before a clean btrfs umount.
- For an operator-triggered `systemctl stop braid-online.service` mid-
  recovery, manual `cryptsetup close` is the only remaining cleanup path.

The sibling `run_plain_lock` (`cli/src/main.rs:981-1014`) hits the same
hard-exit at line 1000 for the same reason.

The cleanup inside `cmd_lock` (`cli/src/lock.rs:960-994`) is already
membership-tolerant. `Snapshot::Probed`'s Pass 1
(`cli/src/lock.rs:813-829`) and `Snapshot::Unmounted`'s
`build_close_sets_uuid_scanned_fallback` (`cli/src/lock.rs:919-958`) classify
every observed `braid-*` mapper as orphan when membership is empty, and each
orphan is verified by `cryptsetup status` + `luksUUID` against the backing
device before being closed. Membership only adds the "member-owned" label
for status output; the per-candidate LUKS UUID probe is the fail-closed
guard. Loosening the load contract for `lock` is therefore safe.

The intended outcome: both lock paths fall back to `PoolMembership::empty()`
on any `MembershipError` load failure, emit a stderr warning naming the
failure, and proceed. Every other braid command (`unlock`, `add`, `recover`,
`remove`, `remove-missing`, `replace`, `enroll`, `discover --write`) keeps
the existing hard-exit behavior, because they all *write* membership or need
it for identity input.

## Approach

Introduce one shared helper in `cli/src/main.rs`:

```rust
/// Lenient pool.json loader for `lock` paths only. `lock` is the only
/// command for which pool.json is non-authoritative -- the per-candidate
/// `cryptsetup luksUUID` probe in `build_close_sets_full` and
/// `build_close_sets_uuid_scanned_fallback` (cli/src/lock.rs) is the
/// fail-closed guard, so empty membership still produces a correct
/// teardown. Returns the loaded membership on success, or empty membership
/// with a stderr warning naming the failure mode.
fn load_membership_for_lock(paths: &StatePaths) -> PoolMembership
```

Call it from `run_plain_lock` (replacing `cli/src/main.rs:1000`) and
`run_systemd_stop_lock` (replacing `cli/src/main.rs:1061`). No other call
sites change.

`load_config_or_exit` (`cli/src/main.rs:961-969`) stays fatal: config is
NixOS-module-generated (`modules/braid/cli.nix`) and not user-touchable.

## Code changes

### `cli/src/main.rs`

1. Add `load_membership_for_lock` right after `load_membership_or_exit`
   (after line 979). Match on every `MembershipError` variant and emit a
   variant-specific stderr line; `Save` is unreachable on the read path and
   should `unreachable!()` to keep the variant exhaust honest.

   Wording (one line per variant, single shared trailing clause so the
   journal grep target is stable):

   - `MembershipError::Io { path, source }`:
     `warn: pool.json unreadable at <path>: <source> -- proceeding with empty membership; every observed braid-* mapper will be verified by LUKS UUID before close`
   - `MembershipError::Corrupt { path, detail }`:
     `warn: pool.json corrupt at <path>: <detail> -- proceeding with empty membership; ...` (same trailing clause)
   - `MembershipError::Conflict(msg)`:
     `warn: pool.json conflict: <msg> -- proceeding with empty membership; ...`
   - `MembershipError::DuplicateDevid { devid, members }`:
     `warn: pool.json duplicate devid <devid> across members <members> -- proceeding with empty membership; ...`
   - `MembershipError::Save { .. }` -> `unreachable!("load_membership cannot return Save")`.

   Use bare `eprintln!`, not `print_cli_error` (which prefixes `error:`).
   The stderr stream is captured by systemd's journal for ExecStop and by
   the operator's terminal for plain `braid lock`.

2. Edit `cli/src/main.rs:1000` (in `run_plain_lock`) from
   `let membership = load_membership_or_exit(paths, 1);`
   to
   `let membership = load_membership_for_lock(paths);`.

3. Edit `cli/src/main.rs:1061` (in `run_systemd_stop_lock`) the same way.

The two lock dry-run sites that still need `pool.json` for the **preview**
(at `cli/src/main.rs:645` for `Lock(args)` when `args.dry_run`) keep using
`load_membership_or_exit`. Dry-run is read-only operator-facing and a hard
error is the right UX -- the user can run `braid discover --write` to
rebuild before previewing.

## Tests

### Rust unit tests

1. **`cli/src/lock.rs`: `cmd_lock_with_empty_membership_closes_observed_orphan_mappers`**
   - Sibling to `fallback_member_named_mapper_with_different_uuid_is_orphan`
     (around `cli/src/lock.rs:2388`).
   - Construct `PoolMembership::empty()` (`cli/src/membership.rs:251`).
   - Build a `MockRunner` with `lock_err_raw("mountpoint -q /mnt/storage", 1, "")`
     so the snapshot is `Unmounted`, plus `with_orphan_mapper(...)`
     (`cli/src/lock.rs:1149` neighborhood) for two mappers `braid-aaa` and
     `braid-bbb`, plus `CryptsetupClose` mocks for both.
   - `lock_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"])` so the
     scan finds the candidates.
   - Drive through `cmd_lock_impl` and assert `Ok(())`, both mappers closed,
     no member-known-closed entries.
   - Preamble must follow the Intent / Why-it-exists / Scenario form
     required by `docs/testing.md` (referenced from `AGENTS.md`).

2. **`cli/src/main.rs`: `load_membership_for_lock_returns_empty_on_load_failure`**
   (one test per variant; keep them small).
   - Use `tempfile::TempDir` for the state dir.
   - **Io case** -- do not create `pool.json`, call the helper, assert the
     returned membership is empty.
   - **Corrupt case** -- write `pool.json` with `{not valid json}`, call
     the helper, assert empty.
   - **Conflict case** -- write a syntactically valid `pool.json` that
     trips the load-time value-side uniqueness sweep
     (`cli/src/membership.rs:454-477`). Two UUID-keyed entries with the
     same `name` field is the smallest case (returns
     `MembershipError::Conflict`). Call the helper, assert empty.
   - `DuplicateDevid` is not separately tested; its code path is identical
     to the other three (`return PoolMembership::empty()`) and differs only
     in warning wording (which is not asserted on).
   - The contract under test is the return value (`empty()` regardless of
     which variant). Skip asserting on the exact warning string; that is
     journal/operator surface, not a structural contract.

### VM test

3. **`tests/module/lock-tolerates-missing-pool-json.py`** (+ `.nix`)
   - Use `tests/module/execstop-cleans-stale-online.{py,nix}` as the verbatim
     template (read above to confirm the structure).
   - Single VM boot exercises BOTH dispatch arms back-to-back. Both
     `run_plain_lock` (`cli/src/main.rs:1000`) and `run_systemd_stop_lock`
     (`cli/src/main.rs:1061`) must be swapped to `load_membership_for_lock`;
     a single VM boot with both subtests catches an implementer who edits
     only one call site.
   - Preamble per `AGENTS.md` "Test Conventions":
     - Intent: both `braid lock` (plain operator path) and
       `braid lock --systemd-stop` (`braid-online.service` ExecStop reentry)
       clean up open `/dev/mapper/braid-*` mappers when
       `/var/lib/braid/pool.json` is missing.
     - Why it exists: protects the documented `braid discover --write`
       recovery workflow -- the operator may legitimately leave
       `pool.json` missing while `braid-online.service` is still active,
       and both the operator-triggered and shutdown-triggered cleanup
       paths must complete. Catches an implementer who only swaps one of
       the two dispatch arms to the new helper.
     - Scenario: pool unlocks normally; `pool.json` is moved aside; the
       operator first runs `braid lock` directly, then re-unlocks, moves
       `pool.json` aside again, and lets `braid-online.service` ExecStop
       handle teardown.
   - Script:
     1. `printf %s\\\\n testpassphrase | braid unlock --passphrase-stdin`
     2. Assert `mountpoint -q /mnt/storage` and
        `systemctl is-active --quiet braid-online.service`.
     3. `mv /var/lib/braid/pool.json /var/lib/braid/pool.json.away`.

     **Subtest A: plain `braid lock`** (covers `run_plain_lock` arm)
     4. `braid lock` -- must succeed (rc 0).
     5. Assert `! ls /dev/mapper/braid-* 2>/dev/null` (all mappers closed).
     6. Assert `! mountpoint -q /mnt/storage` (pool unmounted).
     7. Assert `! systemctl is-active --quiet braid-online.service`
        (plain lock's `mark_offline` deactivated the unit).
     8. Assert stderr (captured from step 4) contains
        `pool.json unreadable` -- pins the warn-was-emitted contract.

     **Subtest B: `braid-online.service` ExecStop** (covers `run_systemd_stop_lock` arm)
     9. Restore: `mv /var/lib/braid/pool.json.away /var/lib/braid/pool.json`.
     10. `printf %s\\\\n testpassphrase | braid unlock --passphrase-stdin`
         (re-unlock to bring `braid-online.service` back to active).
     11. Assert `systemctl is-active --quiet braid-online.service`.
     12. `mv /var/lib/braid/pool.json /var/lib/braid/pool.json.away` again.
     13. `systemctl stop braid-online.service` -- must succeed (job result
         `done`, not `exit-code`).
     14. Assert `! ls /dev/mapper/braid-* 2>/dev/null` (mappers closed).
     15. Assert `! systemctl is-active --quiet braid-online.service`.
     16. `journalctl -u braid-online.service --no-pager -o cat | grep -q 'pool.json unreadable'`
         -- pins the warn-was-emitted contract for the ExecStop path.
   - Register in `flake.nix` adjacent to `execstop-cleans-stale-online`
     (`flake.nix:732-736`), same `pkgs.testers.nixosTest` + `linuxCrane.braid-cli-unwrapped`
     shape.

### Scope of skipped coverage

No `DuplicateDevid` variant test. Helper-level unit test #2 above
covers `Io`, `Corrupt`, and `Conflict`; `DuplicateDevid` takes the same
code path (`return PoolMembership::empty()`) and differs only in warning
wording, which is not asserted on.

## Documentation updates

1. `docs/decisions/017-runtime-disk-membership.md:62-69` (State contract).
   - Reword the "if pool.json is missing or corrupt" bullet so it's
     explicit that `unlock` (and the other mutators) fail, while
     non-dry-run `braid lock` does NOT fail. Add a single new bullet,
     e.g.:
     > Non-dry-run `braid lock` (the user-facing `braid lock` command
     > and the `braid-online.service` ExecStop reentry) tolerates a
     > missing or corrupt `pool.json`: it warns and proceeds with empty
     > membership. The per-candidate `cryptsetup luksUUID` probe in
     > `build_close_sets_*` (cli/src/lock.rs) is the fail-closed guard,
     > so cleanup remains complete and correct. `braid lock --dry-run`
     > still requires a loadable `pool.json` to render its preview and
     > exits with the standard load error otherwise.

2. `docs/luks-unlock.md:143-170` (Unparseable state-file reconciliation).
   - After the corrupt-pool.json paragraph (around line 164), add a
     sentence:
     > Note: non-dry-run `braid lock` (the user-facing `braid lock` and
     > the `braid-online.service` ExecStop path) does NOT fail under a
     > missing or corrupt `pool.json`; it warns and proceeds with
     > per-mapper LUKS-UUID verification so shutdown cleanup remains
     > complete. `braid lock --dry-run` is the exception: the preview
     > pathway still requires a loadable `pool.json`.

No new decision doc. The safety argument is already in
`cli/src/lock.rs:813-816` and `:935-936`; the contract loosening is small
enough for an in-place doc edit.

## Anti-scope

Explicitly NOT included:

- `load_config_or_exit` stays fatal in both lock paths
  (`cli/src/main.rs:999, :1060`). Config is module-generated.
- No change to `cmd_unlock`, `cmd_add`, `cmd_recover`, `cmd_remove`,
  `cmd_remove_missing`, `cmd_replace`, `cmd_enroll_key_file`,
  `cmd_discover`, `cmd_status`, `cmd_monitor`, or any other command that
  reads `pool.json`. Their `load_membership_or_exit` calls remain.
- No change to `build_close_sets_full`, `build_close_sets_uuid_scanned_fallback`,
  `push_uuid_classified_candidate`, or any other lock-internal verification
  logic.
- No change to the wire format or schema of `pool.json`. No migration
  path. No new fields.
- No change to `disk_name_candidates` (`cli/src/main.rs:1078-1089`); tab
  completion already silently returns an empty list on missing pool.json,
  which is correct.
- No change to the `Lock(args)` dry-run arm (`cli/src/main.rs:642-652`).
  Dry-run is operator-facing read-only; the existing hard error directing
  the user to `braid discover --write` is the right UX.

## Critical files

- `cli/src/main.rs` -- new helper after line 979; two call-site swaps at
  lines 1000 and 1061.
- `cli/src/lock.rs` -- new unit test.
- `cli/src/membership.rs:251` -- existing `PoolMembership::empty()` (no
  change, just consumed).
- `tests/module/lock-tolerates-missing-pool-json.py` (new).
- `tests/module/lock-tolerates-missing-pool-json.nix` (new).
- `tests/module/execstop-cleans-stale-online.{py,nix}` -- template (no
  change).
- `flake.nix` -- new `checks` entry next to `execstop-cleans-stale-online`
  at line 732.
- `docs/decisions/017-runtime-disk-membership.md` -- bullet edit.
- `docs/luks-unlock.md` -- one-sentence addition.

## Verification

```sh
just test-rust                                          # unit tests
just test-vm lock-tolerates-missing-pool-json           # the new VM test (covers both arms)
just test-vm execstop-cleans-stale-online               # regression: original ExecStop path
just test-vm braid-lock-systemd-stop                    # regression: deadline-expiry behavior
just test-vm                                            # full module-test suite
```

Manual journal inspection inside the new VM test confirms the
`pool.json unreadable` warn string surfaces:

```sh
journalctl -u braid-online.service --no-pager -o cat | grep 'pool.json unreadable'
```

## Implementation notes

- The existing lock loader already had a bootstrap-add journal fallback for
  missing `pool.json`; this implementation replaces that special case with the
  plan's empty-membership fallback and updates the lifecycle ADR plus the
  bootstrap-failure test comments so they no longer document the removed
  journal path.
