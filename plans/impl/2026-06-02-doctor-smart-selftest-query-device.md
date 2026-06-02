# Fix doctor.md SMART self-test query-device claim

## Context

`docs/commands/doctor.md` claims the SMART self-test check runs
`smartctl --json -A -l selftest <by-id>` (line 81 table row, line 104
step 4). That is wrong for the common case. `check_smart_selftests`
(`cli/src/doctor.rs#check_smart_selftests`) resolves the query device as:

```rust
let query_device = live
    .and_then(|pool| pool.underlying_for_uuid(uuid))
    .unwrap_or(by_id);
```

`live` is `Some` only when the pool is mounted and `probe_pool` succeeds
(`cli/src/doctor.rs#ensure_pool_state`), and
`PoolState::underlying_for_uuid` (`cli/src/types.rs#PoolState::underlying_for_uuid`)
returns the device's live backing path (e.g. `/dev/sda`). So on a mounted
pool braid reads the self-test log from the **live backing device**, and
only falls back to the persisted **by-id** path when the pool is offline,
the probe failed, or the member is absent from live topology. The
`--json -A -l selftest` flags themselves are accurate
(`cli/src/cmd.rs`, `CmdRequest::SmartctlSelftestLogJson` -> argv); only the
`<by-id>` device placeholder is wrong.

A reader troubleshooting a self-test row cannot reproduce the exact
invocation braid ran on a mounted pool. The doc implies the by-id symlink
is always the target; in practice braid passes the resolved underlying
device.

Intended outcome: the two query descriptions name the real target, and the
doc preempts the inverse confusion -- braid's diagnostic *read* uses the
live device, but the separate paste-ready *hint* (`smartctl -t short ...`,
line 46) intentionally stays on by-id because by-id is the stable handle a
human types later. This is a deliberate split, not an inconsistency:
querying the authoritative live source matches the "query the authoritative
source of state directly" mutation-safety heuristic, while by-id survives
reboots/controller reordering for a recommended future command.

## Scope

Documentation only. **No code change** -- the runtime behavior is correct
and intentional. One file: `docs/commands/doctor.md`. Three edits.

## Edits

### 1. Table row (line 81) -- name the real query device

Replace the opening fragment:

> One result per pool drive: runs `smartctl --json -A -l selftest <by-id>` against each, then reports `Fail` on ...

with:

> One result per pool drive: runs `smartctl --json -A -l selftest <device>` against each -- `<device>` is the member's live backing device (e.g. `/dev/sda`) when it is assembled into the mounted pool, otherwise its persisted by-id path (pool offline, probe failed, or that member not currently assembled -- e.g. missing or hot-unplugged on a degraded mount) -- then reports `Fail` on ...

(The rest of the cell -- `Warn`/`Ok`/`Skip` semantics, `--json` fallback
behavior, `subject` field -- is unchanged.)

### 2. "Under the hood" step 4 (line 104) -- same correction

Replace:

> 4. For each declared disk, runs `smartctl --json -A -l selftest <by-id>` and parses the self-test log to detect active failures and report the age of the most recent passing entry.

with:

> 4. For each declared disk, runs `smartctl --json -A -l selftest <device>` -- the member's live backing device when it is assembled into the mounted pool, otherwise its persisted by-id path (including a member that is missing or unassembled on a degraded but mounted pool) -- and parses the self-test log to detect active failures and report the age of the most recent passing entry. See [ADR-024](../design/decisions/024-luks-uuid-identity.md#benefits) for why present members are probed by live path rather than by-id.

(The `See [ADR-024](...#benefits)` cross-link mirrors the existing
`docs/commands/add.md` -> ADR-027 and `docs/commands/idle.md` -> ADR-016
links; `#benefits` is the slug for ADR-024's `## Benefits` heading, whose
"Present-device hardware probes use live paths" bullet names smartctl and
`PoolState::underlying_for_uuid` directly. It is `mdbook-linkcheck2`-validated.)

### 3. Hint section (after the example block, ~line 47) -- preempt the inverse finding

The example warn row at line 46 already shows the correct by-id `-t short`
hint and stays as-is. Add ONE new paragraph immediately after the closing
fence of that example code block:

> The hint uses the stable by-id path: braid's own diagnostic read prefers the member's live backing device, but a `smartctl -t short` you run later should use by-id, which survives reboots and controller reordering.

("prefers ... otherwise by-id" rather than a flat "reads from the live
device", so edit #3 agrees with #1/#2 instead of re-asserting the
mounted-equals-live imprecision they just removed.)

## Deliberately unchanged

- `docs/commands/doctor.md:46` -- the example warn row. It is the `-t short`
  *hint* (start a test), a different operation from the `-l selftest` *read*,
  and by-id is correct there.
- `cli/src/doctor.rs`, `cli/src/types.rs`, `cli/src/cmd.rs` -- behavior is
  correct; the finding is a doc-accuracy issue, not a code bug.
- `README.md` -- has no SMART self-test command claim (verified by grep).
- `docs/guides/fan-control.md:261` (`smartctl -a /dev/sda`) -- unrelated
  temperature read, not the self-test query.
- `docs/book/**` -- gitignored mdbook build output; regenerated, never
  hand-edited.

## Style notes

- Phrase the device rule as live-pool *assembly*, not mere mount state, and
  do not "simplify" it back to "when mounted". On a mounted-but-degraded
  pool a missing/hot-unplugged/locked member has no entry in
  `PoolState::devices` (`cli/src/probe.rs#probe_pool` pushes null-underlying
  members to `null_underlying` and `continue`s; MISSING members never appear
  as mapper paths), so `PoolState::underlying_for_uuid` returns `None` and
  `query_device` (`cli/src/doctor.rs#check_smart_selftests`) falls back to
  by-id *even though the pool is mounted*. ADR-024's `## Offline Disk State`
  section is the authority for this "not assembled into the live btrfs pool"
  state. "When mounted" alone mis-names the device for exactly the sick-pool
  case doctor exists to diagnose.
- Use `--` (ASCII), never em-dash, per project CLI/doc style.
- Keep "live backing device (e.g. `/dev/sda`)" wording consistent with
  `docs/commands/status.md`'s description of the `underlying` field
  ("current backing block device (e.g. `/dev/sda`)").
- Do not introduce line-number cross-references; the edits add none.

## Verification

1. `mdbook build docs` -- must succeed. The step-4 edit adds a real
   cross-link to `024-luks-uuid-identity.md#benefits`, so `mdbook-linkcheck2`
   now actively validates that target heading still exists (it fails CI if
   the ADR's `## Benefits` heading is ever renamed).
2. Manual read-back: re-read lines 81, 104, and the hint paragraph against
   `cli/src/doctor.rs#check_smart_selftests` and `cli/src/probe.rs#probe_pool`
   to confirm the prose matches
   `query_device = live...underlying_for_uuid(uuid).unwrap_or(by_id)` for the
   assembled case *and* the mounted-but-unassembled fallback to by-id.
3. No Rust/VM tests are affected (no test asserts on this prose; the
   `smart_self_test` parser/behavior tests are untouched). Do not run the
   VM suite for a doc-only change.
