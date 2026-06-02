# Plan: document the missing top-level `braid status --json` fields

## Context

The JSON output section of `docs/commands/status.md` advertises itself as "a
structured report suitable for monitoring tools," but its field list omits five
fields that `StatusReport` actually serializes (`cli/src/status.rs#StatusReport`):

- `mount_point` -- serialized unconditionally
- `total_devices`, `present_count`, `missing_count`, `fsid` -- serialized when
  the pool is mounted (`skip_serializing_if = "Option::is_none"`)

Two consequences:

1. A consumer reading the doc never learns these fields exist.
2. The existing `missing_devids` bullet (`docs/commands/status.md`, JSON output
   section) is defined as "every devid counted in `missing_count`" -- but
   `missing_count` is never defined anywhere in the section. The reference
   dangles.

The human-readable "Pool summary" section already documents the same values
(`Pool:` = `mount_point`, `FSID:` = `fsid`); only the JSON section drifted. This
plan closes that gap and, while we're there, adds the section's first complete
top-level example envelope so the whole shape is concrete for monitoring
consumers and future drift is visually obvious.

Scope: `docs/commands/status.md` (the schema doc) **plus one regression test**
in `cli/src/status.rs` that pins the mounted top-level key set. No production
code changes.

### Why no other docs need updating

- It is the only place that enumerates the `status --json` top-level schema. No
  `*.schema.json` exists; `README.md` is cookbook-style and does not document
  the schema; `docs/guides/mounting-subvolumes.md` only mentions that the `fsid`
  field exists (a usage hint, not a schema list). None need updating.
- The JSON output section has no sub-headings, and the only incoming mdBook
  cross-links target `status.md#pending-luks-header-backups` (from `add.md`,
  `enroll.md`, `recover.md`, `replace.md`) -- a heading outside our edit region.
  Nothing we add or touch affects linkcheck.

## Source of truth (verified)

Serialized top-level fields, declaration order (`cli/src/status.rs#StatusReport`):
`mount_point`, `status`, `total_devices?`, `present_count?`, `missing_count?`,
`profile?`, `fsid?`, `capacity?`, `last_scrub?`, `balance?`, `allocation?`,
`disks`, `advisories(skip-if-empty)`, `alert_active`,
`alert_causes(skip-if-empty)`, `missing_devids(skip-if-empty)`.

Presence contracts, all test-pinned:

- Not-mounted **baseline** is `{ mount_point, status, disks, alert_active }`
  -- `not_mounted_status (cli/src/status.rs#not_mounted_status)` sets the four
  mounted-only Options to `None`. This minimal 4-key shape (empty advisories, no
  latched alert) is pinned by `not_mounted_status_envelope_is_minimal` and
  `status_json_not_mounted` (both assert `obj.len() == 4`). It is **not** an
  invariant: `advisories` and `alert_causes` still serialize when non-empty
  (skip-if-empty), so an offline pool with a latched alert or a pending-op
  journal carries extra keys -- pinned by
  `build_status_surfaces_pending_op_advisory_when_not_mounted` (a 5-key offline
  envelope). `missing_devids` is always empty offline, so it stays omitted.
- Mounted path sets all four to `Some` (`cli/src/status.rs#build_status`):
  `present_count = total_devices - missing_count` (`saturating_sub`).
- `missing_count == missing_devids.len()` -- pinned at `cli/src/status.rs`
  (test asserting `missing_devids.len() == missing_count.unwrap()`).
- `fsid` is `Some` whenever the pool is mounted: `probe_pool`
  (`cli/src/probe.rs`) errors out rather than returning a null fsid -- "A
  mounted btrfs filesystem always has an FSID." So "present when mounted" is an
  accurate, unconditional statement, not best-effort.
- `mount_point` serializes as a plain string (pinned: `obj["mount_point"] == "/mnt/storage"`).

Nested object shapes for the example (verified against the types):

- `balance` = `BalanceReport` (`cli/src/status.rs#BalanceReport`), internally
  tagged on `state`: `{ "state": "idle" }`, or `running`/`paused` with
  `done_chunks`/`estimated_total_chunks`/`considered_chunks`/`pct_left`, or
  `{ "state": "unknown" }`.
- `last_scrub` = `ScrubReport` (`cli/src/status.rs#ScrubReport`), internally
  tagged on `state`: `finished`/`aborted`/`interrupted` carry `started_at`
  (offset-free ISO-8601 `YYYY-MM-DDTHH:MM:SS`) and `error_count`; `running`
  carries optional `pct`; `never`/`unknown` are bare. (`started_at_human` and
  `journal_since` are `#[serde(skip)]` -- not in JSON.)
- `capacity`, `allocation`, `profile`, `disks[]` shapes are already documented
  in the section; reuse them verbatim.

## Edits to `docs/commands/status.md` (JSON output section only)

### 1. Add the five field bullets, grouped at the top of the list

Insert a `mount_point` bullet **before** the existing `status` bullet, and the
four mounted-only scalar bullets **immediately after** `status` and before the
`disks` bullet. This groups the always-present pair first, then the mounted-only
pool scalars -- and crucially places `missing_count`'s definition *before* the
`missing_devids` bullet that references it, dissolving the dangling reference.

Use the section's established conditional-presence house style (bold the
keyword: **Always present** / **Present when ...; omitted ...**):

- `mount_point`: the pool's configured mount path (e.g. `/mnt/storage`) -- the
  same value shown on the human-readable `Pool:` line. **Always present**, in
  both the mounted and not-mounted envelopes.
- *(existing)* `status`: `"intact"`, `"degraded"`, or `"not_mounted"`
- `total_devices`: total number of devices btrfs reports for the pool, as a
  number. **Present when the pool is mounted; omitted in the not-mounted
  envelope.**
- `present_count`: number of member devices currently present, equal to
  `total_devices - missing_count`, as a number. **Present when the pool is
  mounted; omitted in the not-mounted envelope.**
- `missing_count`: number of member devices counted as missing -- the
  cardinality of the `missing_devids` array below (btrfs-MISSING devices plus
  null-underlying mappers whose backing device disappeared); `0` on a healthy
  pool. **Present when the pool is mounted; omitted in the not-mounted
  envelope.**
- `fsid`: the btrfs filesystem UUID, as a string -- the same value shown on the
  human-readable `FSID:` line, and distinct from a disk's `luks_uuid`.
  **Present when the pool is mounted** (a mounted btrfs filesystem always has an
  FSID); omitted in the not-mounted envelope.

### 2. Add a complete top-level example envelope at the end of the section

After the `last_scrub` bullet and before `## Related commands`, add the
section's first full top-level example, introduced with a prose lead-in (the
section's convention for code blocks -- no new heading). Field order matches
serialization order; empty `advisories`/`alert_causes`/`missing_devids` are
omitted (skip-if-empty), as they are on a healthy pool:

A complete report for a healthy 3-disk RAID1 pool:

```json
{
  "mount_point": "/mnt/storage",
  "status": "intact",
  "total_devices": 3,
  "present_count": 3,
  "missing_count": 0,
  "profile": {
    "data": ["RAID1"],
    "metadata": ["RAID1"],
    "system": ["RAID1"]
  },
  "fsid": "f5f5f5f5-aaaa-bbbb-cccc-d0d0d0d0d0d0",
  "capacity": {
    "total_bytes": 18000000000000,
    "used_bytes": 6000000000000,
    "free_bytes": 12000000000000
  },
  "last_scrub": {
    "state": "finished",
    "started_at": "2026-05-01T03:00:00",
    "error_count": 0
  },
  "balance": { "state": "idle" },
  "allocation": [
    { "bg_type": "Data", "profile": "RAID1", "used_bytes": 6000000000000, "allocated_bytes": 6500000000000 },
    { "bg_type": "Metadata", "profile": "RAID1", "used_bytes": 8000000000, "allocated_bytes": 9000000000 },
    { "bg_type": "System", "profile": "RAID1", "used_bytes": 65536, "allocated_bytes": 33554432 }
  ],
  "disks": [
    {
      "name": "toshiba1",
      "mapper": "braid-toshiba1",
      "by_id": "/dev/disk/by-id/ata-TOSHIBA_MN07ACA12T_1234",
      "luks_uuid": "aaaaaaaa-1111-2222-3333-444444444444",
      "devid": 1,
      "underlying": "/dev/sda",
      "status": "present",
      "errors": { "read": 0, "write": 0, "flush": 0, "corruption": 0, "generation": 0 }
    }
  ],
  "alert_active": false
}
```

> When the pool is not mounted, every mounted-only field above (`total_devices`,
> `present_count`, `missing_count`, `profile`, `fsid`, `capacity`, `last_scrub`,
> `balance`, `allocation`) is omitted, leaving `mount_point`, `status`
> (`"not_mounted"`), `disks` (`[]`), and `alert_active`. `advisories` and
> `alert_causes` still follow their skip-when-empty rule, so a latched alert or a
> pending-operation advisory can still appear on an offline pool.

Notes for the implementer:

- `fsid` in the example is deliberately visually distinct from the disk's
  `luks_uuid` to reinforce the "distinct from `luks_uuid`" bullet.
- The `disks` array shows one element to match the existing `disks[]` example;
  one element is enough.
- Leave the existing `balance` and `last_scrub` bullets as-is; the new example
  illustrates their `state`-tagged shape without expanding those bullets
  (keeps scope to the five fields + envelope).

## Regression test (`cli/src/status.rs`)

The five-field doc gap arose because nothing pins the mounted top-level key set.
`status_json_healthy` (`cli/src/status.rs`) asserts individual mounted keys but
never the exact set or a count, and is hand-built with `balance: None` -- not
even the production mounted shape. Only the not-mounted envelope is key-set
pinned (`not_mounted_status_envelope_is_minimal`, `status_json_not_mounted` --
both assert `obj.len() == 4`). So adding a field to `StatusReport` trips no test,
and the doc drifts again silently -- exactly how the current gap formed. A
hand-maintained example envelope is *more* drift surface, not less, unless the
contract is enforced in CI; "drift is visually obvious" only helps a reader who
reopens the doc.

Add one behavioral, structure-insensitive test mirroring
`not_mounted_status_envelope_is_minimal`, driving the production helper
`build_healthy_status()` (`cli/src/status.rs#build_healthy_status`, already in the
test module) and asserting the *exact* sorted top-level key set:

```rust
// Intent: pin the exact top-level key set of a healthy mounted `braid status
//   --json` report, so adding/removing a StatusReport field is a CI failure
//   rather than silent JSON-schema drift.
// Why it exists: the docs/commands/status.md JSON section is a hand-maintained
//   mirror of StatusReport; five fields drifted undocumented because only the
//   not-mounted envelope was key-set pinned. On failure, update BOTH this set
//   and the docs/commands/status.md JSON output section.
// Scenario: a healthy 3-disk RAID1 pool, all tools succeeding, no advisories or
//   alerts -- the canonical mounted report a monitoring consumer parses.
#[test]
fn mounted_status_envelope_top_level_keys_are_pinned() {
    let built = build_healthy_status();
    let v: serde_json::Value =
        serde_json::from_str(&serde_json::to_string(&built.report).unwrap()).unwrap();
    let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "alert_active", "allocation", "balance", "capacity", "disks",
            "fsid", "last_scrub", "missing_count", "mount_point",
            "present_count", "profile", "status", "total_devices",
        ],
    );
}
```

The 13-key set is the healthy mounted contract: `mount_point`/`status`/`disks`/
`alert_active` (always present) plus the mounted-only `total_devices`/
`present_count`/`missing_count`/`profile`/`fsid`/`capacity`/`last_scrub`/
`balance`/`allocation` -- a healthy `build_healthy_status()` run yields all nine
(confirmed by `assert_pool_sections_retained` and
`assert_scrub_and_balance_retained` in the same module). The skip-if-empty
fields `advisories`/`alert_causes`/`missing_devids` are absent on a healthy pool.

Known limitation, shared with the not-mounted pin: a *new* skip-if-empty field
that is empty in this fixture would not trip the test, so the doc remains the
home for documenting such fields. The test still guards the common case -- any
new always-serialized field, or any removed/renamed field, fails CI.

Place it next to `not_mounted_status_envelope_is_minimal` so the two envelope
pins sit together; `build_healthy_status()` is in the same `#[cfg(test)]` module
and callable regardless of definition order.

## Out of scope

- No change to `README.md` (no JSON schema there) or
  `docs/guides/mounting-subvolumes.md` (fsid usage hint only, still accurate).
- No production code changes beyond the one regression test above; do not run
  any formatter.

## Verification

1. `mdbook build docs` -- renders the page and runs `mdbook-linkcheck2` (the CI
   gate). Confirms no broken cross-links; we add none and rename nothing.
2. Cross-check every field name/value in the new bullets and example against the
   source of truth: `cli/src/status.rs#StatusReport` and the nested
   `BalanceReport`/`ScrubReport`/`CapacityReport`/`AllocationEntry`/`DiskReport`/
   `DiskErrors`/`ProfileJson` types. The pre-existing tests pin only part of the
   shape -- `status_json_healthy` asserts individual mounted keys but not the
   exact set (and is hand-built with `balance: None`), while
   `not_mounted_status_envelope_is_minimal` / `status_json_not_mounted` pin the
   minimal not-mounted key set. The mounted key set was unguarded; the new
   regression test above closes that gap.
3. `just test-rust` -- runs the new
   `mounted_status_envelope_top_level_keys_are_pinned` test plus the existing
   status JSON tests. This is the enforced contract; the doc and example are its
   human-readable view.
4. Optional strongest check, if a dev/VM pool is available:
   `braid status --json | jq 'keys'` on a mounted pool and on an unmounted pool,
   and confirm the key sets match the example envelope and the not-mounted note
   respectively.
