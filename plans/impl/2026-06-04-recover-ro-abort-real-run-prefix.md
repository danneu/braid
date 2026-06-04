# Plan: pin the real-run prefix in recover's read-only abort tests

## Context

A Low/Testing review finding claimed the read-only abort in the real-run
completion path (`cli/src/recover.rs#RecoverCompletion::execute`, the
`entry_is_read_only(&entry)` arm at `cli/src/recover.rs:578`) had no test
driving `cmd_recover` -- only the dry-run planner branch was covered, so a
refactor that dropped the completion-path check would rebuild `pool.json`
and clear the journal against a btrfs that auto-remounted read-only after
an I/O error, masking a failing device.

A `/verify-issue` pass found the headline claim **stale**. Two real-run
tests, added in the *same* commit as the check (`f885d49d "fix(recover):
refuse completion on read-only pools"`), already drive `cmd_recover` (not
`plan_recover`) with a read-only mount and assert exactly what the finding
asked for -- error text, `pool.json` unwritten, journal preserved -- across
both read-only flavors:

- `cmd_recover_aborts_when_post_mount_probe_reports_vfs_read_only`
  (`cli/src/recover.rs:16417`) -- VFS-options read-only
  (`MockFs::with_mounted_pool_ro_vfs`)
- `cmd_recover_aborts_when_post_mount_probe_reports_fs_read_only`
  (`cli/src/recover.rs:16465`) -- mountinfo field-11 read-only
  (`MockFs::with_mounted_pool_ro_fs`)

These run with `dry_run = false` (the builder default, `cli/src/test_fixtures/recover.rs:57`),
and the dry-run RO check is gated `if open_plan.is_none() && params.dry_run`
(`cli/src/recover.rs:1338`), so in these tests it is structurally
unreachable -- the only check that can fire is `execute()`'s. Both pass today.

So there is **no coverage gap**. The single residual weakness is assertion
precision: both real-run tests assert only `msg.contains("mounted
read-only")`, a substring shared by *both* RO error strings -- the execute()
one (`"recovery aborted: pool at ... is mounted read-only ..."`,
`cli/src/recover.rs:580`) and the dry-run one (`"recover dry-run: pool at
... is mounted read-only ..."`, `cli/src/recover.rs:1358`). The dry-run test
pins its own path with `err.contains("recover dry-run")`
(`cli/src/recover.rs:17813`); the real-run tests have no symmetric pin on the
real-run prefix. They are correct today only by construction (the dry-run
branch can't fire), not by assertion.

The ideal fix is a small, idiomatic test hardening: pin the distinct
real-run prefix in the two existing tests, mirroring the dry-run test, so
the intent is explicit and survives a future change to the dry-run gating.
This is deliberately *not* a refactor of the two error strings: they differ
in tense and framing on purpose (execute() reports what already happened --
"pool.json was not written ... is preserved"; dry-run reports the
conditional -- "execute would refuse ... are unchanged"), so collapsing
them into one template would be worse, not better.

## Change

File: `cli/src/recover.rs` (test module only -- no production code changes).

In each of the two tests, add one assertion immediately *after* the existing
`msg.contains("mounted read-only")` assert, matching the dry-run test's
ordering (read-only state, then path prefix, then remount guidance):

```rust
assert!(
    msg.contains("recovery aborted"),
    "error must identify the real-run completion refusal, not the dry-run wording: {msg}"
);
```

Resulting assertion order in each test: `"mounted read-only"` ->
`"recovery aborted"` (new) -> `"remount,rw"` -> `!pool_json().exists()` ->
`pending_op_json().exists()`.

No other edits. The dry-run test (`cli/src/recover.rs:17776`) already pins
its path and is the template being mirrored; leave it unchanged.

### Why `"recovery aborted"` is the right token

`"recovery aborted"` is the real-run prefix (`cli/src/recover.rs:580`),
symmetric to the dry-run test's `"recover dry-run"`. It is *also* shared with
execute()'s zero-devices abort (`cli/src/recover.rs:608`), so on its own it
would not isolate the RO arm -- but paired with the pre-existing
`"mounted read-only"` assert (which the zero-devices error lacks) the two
substrings together uniquely identify the execute()-path read-only refusal:

| Error path                                   | `mounted read-only` | `recovery aborted`        |
| -------------------------------------------- | ------------------- | ------------------------- |
| execute() read-only (`:580`, **target**)     | yes                 | yes                       |
| execute() zero-devices (`:608`)              | no                  | yes                       |
| dry-run read-only (`:1358`)                  | yes                 | no (says `recover dry-run`) |

Keeping the assertion a plain `contains` substring matches the established
idiom in this test file (the dry-run test asserts `recover dry-run`,
`remount,rw`, etc. the same way) and keeps it behavioral/structure-insensitive
-- no new error variant or matcher machinery is warranted for a wording pin.

## Verification

1. Focused run (iteration):
   `cargo test -p braid-cli --lib cmd_recover_aborts_when_post_mount_probe_reports`
   -- both tests pass with the new assertion.
2. Mutation sanity (manual, revert after): change execute()'s prefix at
   `cli/src/recover.rs:580` from `"recovery aborted:"` to the dry-run wording
   `"recover dry-run:"`. Confirm both tests now **fail** on the new
   `"recovery aborted"` assert while the old `"mounted read-only"` assert
   still passes -- proving the added assertion contributes the path-pinning
   value the prior asserts lacked. Revert the production string.
3. Before handing back: `just test-rust` (full Rust suite) is green.
