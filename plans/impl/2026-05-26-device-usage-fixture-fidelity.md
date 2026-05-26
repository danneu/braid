# Plan: faithful `btrfs device usage --raw` fixture builder + missing-device fidelity

## Context

A review finding flagged that the `remove-missing` relocation-space test
fixtures model a missing btrfs device's `device usage --raw` header as
`<missing disk>, ID: N`, but btrfs-progs never emits that string. Investigation
widened the root cause considerably:

- **The real rendering (pinned btrfs-progs v6.17.1, per `reference/`):**
  - `device usage --raw`: a missing device's header is `missing, ID: N` -- the
    path is the literal word `missing` (`reference/btrfs-progs/cmds/filesystem-usage.c:820-821`,
    printed via `cmds/device.c:1024`). KV lines use a **3-space** indent.
  - `device stats [--format json]`: a missing device renders as `devid:N`
    (`cmds/device.c:625-634,655`) -- JSON `"device": "devid:N"`, text `[devid:N]`.
- `<missing disk>` is a **stale older rendering**. Commit `8e416ca5` shows the
  author knew it was "version-dependent `<missing disk>` or `devid:` rendering"
  and hand-edited fixtures (copied the healthy 2-disk golden and substituted the
  string) rather than capturing real output.
- **No behavioral bug:** every consumer keys on a numeric field, never the
  string -- `check_relocation_space` on `device_size == 0 && devid`
  (`remove_missing.rs:561`), `capacity.rs`/`doctor.rs` enospc on `device_size`,
  and the stats parser/TUI on the JSON `devid` field. The parser's
  `parse_device_header` uses `take_until(",")`, so the path is opaque and both
  strings parse identically.

So this is **fixture fidelity + duplication cleanup**, not a correctness fix.
The fabrication recurred precisely because every device-usage fixture is
hand-written as an independent string (~12 consts across 5 modules, with
`replace.rs` even using tabs instead of spaces), with no single source of truth.

**Outcome:** one shared, faithful `device usage --raw` builder that all
device-usage fixtures route through (so the wrong marker cannot recur and the
format lives in one place), the stale `<missing disk>` markers corrected to the
real pinned-tool strings everywhere (committed golden fixtures and the
tool-behavior reference docs included), and a parser test plus a direct builder
test that pin the missing-device line shape (currently uncovered).

## Decisions (locked with the user)

1. **Full dedup:** introduce the builder and migrate *all* device-usage
   `--raw` fixture strings across `remove`, `replace`, `remove_missing`,
   `doctor`, `status` onto it.
2. **Hand-correct** the committed golden device-stats fixtures to `devid:N`
   (verified against btrfs v6.17.1 source); do **not** run `just capture-all-fixtures`.

## Ground rules

- The parser (`cli/src/parse/btrfs_device_usage.rs`) requires every stanza to
  carry `Device size`, `Device slack`, and `Unallocated` (else `MissingField`);
  it accepts any `Type,Profile: bytes` allocation lines; indentation matches
  nom `space1` (spaces *or* tabs). Column alignment is cosmetic.
- No migrated test asserts on the raw usage text or its whitespace -- all assert
  on parsed `BtrfsDeviceUsageEntry` fields or on downstream
  `CheckResult`/error-message substrings. The tab->space and
  `<missing disk>`->`missing` changes are therefore parse-equivalent and safe.
- `test_fixtures` is `#[cfg(test)]`-gated (`cli/src/lib.rs:59`); the builder is
  test-only.

## 1. The builder (new code in `cli/src/test_fixtures/shared.rs`)

> **Name collision:** `device_usage_raw` is already taken --
> `cli/src/test_fixtures/doctor.rs:215` defines `pub(crate) fn device_usage_raw(stdout: &str) -> (CmdRequest, RawCommandOutput)`
> (re-exported at `cli/src/test_fixtures.rs:147`). Name the new emitter
> **`device_usage_raw_body`** (returns `String`); the existing wrapper composes
> over it: `device_usage_raw(&device_usage_raw_body(&[...]))`.

```rust
/// One device stanza for `device_usage_raw_body`. Mirrors the btrfs
/// `device_info` fields the parser reads so a single struct drives every
/// `btrfs device usage --raw` fixture and stays faithful to btrfs-progs v6.17.1.
pub(crate) struct DeviceUsageSpec {
    /// `Some(path)` for a live device; `None` renders the literal `missing`
    /// header (filesystem-usage.c:820-821).
    pub(crate) path: Option<String>,
    pub(crate) devid: u64,
    pub(crate) device_size: u64,
    pub(crate) device_slack: u64,
    /// (alloc_type, profile, bytes), e.g. ("Data","RAID1",67108864).
    pub(crate) allocations: Vec<(String, String, u64)>,
    pub(crate) unallocated: u64,
}

impl DeviceUsageSpec {
    /// Live device; slack defaults to 0 (every current fixture uses 0).
    pub(crate) fn live(path: &str, devid: u64, device_size: u64,
                       allocations: &[(&str, &str, u64)], unallocated: u64) -> Self { ... }

    /// Missing device: header renders `missing, ID: <devid>`; size/slack
    /// default to 0 (btrfs reports 0 for an absent device). `allocations` and
    /// `unallocated` stay explicit -- a still-referenced missing devid can carry
    /// chunk rows the relocation-space preflight must measure, and Unallocated
    /// is sometimes nonzero.
    pub(crate) fn missing(devid: u64, allocations: &[(&str, &str, u64)],
                          unallocated: u64) -> Self { ... }
}

/// Render `specs` as faithful `btrfs device usage --raw` stdout (v6.17.1:
/// 3-space indent, `missing` marker for absent devices, a blank line after
/// every stanza so output ends `\n\n`). Single source of truth so no fixture re-invents
/// the stale `<missing disk>` rendering and the parser sees real input.
pub(crate) fn device_usage_raw_body(specs: &[DeviceUsageSpec]) -> String { ... }
```

Rendering rule per KV line: `"   {label}{value}\n"` with the value
right-justified into a fixed column (e.g. width `33 - 3 - label.len()`),
matching btrfs's `%*s%10s`. The column width is the implementer's choice but
must be deterministic: the **§1.1 regression test** below freezes the exact
rendered bytes for a canonical input. Column padding aside, exact alignment is
cosmetic (parser uses `space1`); the
hard requirements are the 3-space indent, presence of Device size/slack/
Unallocated, allocation lines in spec order, and **a blank line terminating
every stanza** -- each stanza ends `...Unallocated:...\n`, then a `\n` separator,
so non-empty output ends with `\n\n` (matches `device.c:1027`, which prints `\n`
after every device including the last). Target shape:

```
/dev/mapper/braid-disk1, ID: 1
   Device size:         1073741824
   Device slack:                 0
   Data,RAID1:            52428800
   Unallocated:         1010794496

missing, ID: 3
   Device size:                  0
   Device slack:                  0
   Data,RAID1:            67108864
   Unallocated:                  0

```

The trailing blank line after the final stanza is intentional: btrfs prints `\n`
after every device (`device.c:1027`), so faithful output ends with `\n\n`.

Re-export `DeviceUsageSpec` + `device_usage_raw_body` from the `shared::{...}`
`pub(crate) use` block in `cli/src/test_fixtures.rs`.

### 1.1 Builder regression test (direct fidelity guard)

Every migrated test parses the builder output, and the parser is path-opaque
(`take_until(",")`) and whitespace-tolerant (`space1`). So a builder that
emitted tabs, dropped the final blank line (the `\n\n` ending), lost a
between-stanza separator, mis-ordered allocations, or reintroduced
`<missing disk>` would still pass every
downstream test -- the builder's whole value (faithfulness) would be untested.

Add one focused unit test in `cli/src/test_fixtures/shared.rs` that renders a
canonical `device_usage_raw_body(&[live(/* with an allocation list */), missing(...)])`
and asserts the **exact** rendered string with a single `assert_eq!`. The frozen
string must exhibit: 3-space KV indent, the live `<path>, ID: N` header, the
`missing, ID: N` header (never `<missing disk>`), the required Device size /
Device slack / Unallocated lines, allocation lines in spec order, and a blank
line terminating every stanza so the string ends with `\n\n` (per
`device.c:1027` -- not merely a single trailing newline). This golden assertion pins the builder's
output contract; any intentional format change updates this one test. Follow the
file's test conventions for the `//` preamble.

## 2. Migration inventory (all in scope)

Consts/fns -- convert each `&'static str` const to a `fn -> String` (or build
inline) and change the owning `*Pool` field + handler closure from
`&'static str` to `String` (closures are already `move`):

| File:line | Item | Shape |
|---|---|---|
| `test_fixtures/remove.rs:25,40` | `TWO_/THREE_DISK_USAGE_RAW` | 2/3 live; size 1073741824, Data/Meta/System RAID1 52428800/10485760/32768, Unalloc 1010794496 |
| `test_fixtures/replace.rs:34,45,56` | `PRE_USAGE_RAW_TWO_HEALTHY`, `PRE_USAGE_RAW_ONE_LIVE_MISSING`, `POST_USAGE_RAW_DISK1_DISK3` | **tabs->3-space**; `<missing disk>`->`missing` on devid2; size 520093696, Data/RAID1 469762048, Unalloc 50331648 (0 on missing) |
| `test_fixtures/remove_missing.rs:40,42` | `USAGE_RAW_THREE_/TWO_DISK_ONE_MISSING` | `<missing disk>`->`missing`; live size 520093696, Data/RAID1 67108864, Unalloc 452984832; missing devid Unalloc 0 |
| `test_fixtures/doctor.rs:426,441,456,478` | `DEVICE_USAGE_TWO_HEALTHY/TWO_TIGHT/THREE_ONE_TIGHT/THREE_TWO_TIGHT` | size 10737418240; per-const Data/Meta/System + Unalloc values (preserve exactly) |
| `test_fixtures/status.rs:238,268` | `status_btrfs_device_usage_raw_3disk/_1disk` | 3-disk (paths `/dev/mapper/disk1..3`, no `braid-`) size 346729130; 1-disk single-profile |
| `status.rs:3391` | `status_btrfs_device_usage_raw_3disk_enospc_risk` | same 3-disk shape, lower Unalloc; migrate for full dedup |

Inline test fixtures -- replace the inline string with `device_usage_raw_body(&[...])`:

| File:line | Test | Note |
|---|---|---|
| `doctor.rs:1674` | `enospc_device_usage(&[unalloc], size)` | reduce to a thin adapter that maps to `device_usage_raw_body(&[live(path,i,size,&[],unalloc)..])` (no allocation lines); keeps callers at 4196/4215 unchanged |
| `remove.rs:1819` | survivor-missing | single `live(...)` spec |
| `remove_missing.rs:949,996,1037,1199,1240` | relocation-space tests | preserve each test's exact sizes/allocations; `:1199` lists **no** missing stanza (two `live` only); `:1240` uses `missing(3, &[], 0)` |

## 3. Out of scope -- do NOT migrate (verified)

- **Deliberately-unparseable negative tests:** `doctor.rs:4674` (omits
  Unallocated) and `remove.rs:1721` (truncated) -- the builder always emits the
  required fields, so leave these inline; they assert the parse-error path.
- **`btrfs filesystem usage`** fixtures (`Overall:` blocks): `status.rs:3933`,
  `test_fixtures/status.rs:228`, `parse/btrfs_filesystem_usage.rs` tests --
  different parser.
- **The parser's own inline tests** in `parse/btrfs_device_usage.rs` (lines
  ~201,234,310) -- keep hand-written; coupling them to the builder would be
  circular.
- **`tui/probe.rs` live-usage inlines** (1041, 1143, 1177, 1318, 1331, 1445,
  1564, 1594, 1890, 1909) -- already faithful 3-space live devices, not in the
  5 named modules. Leave to keep the diff focused (note in commit message).
- **TUI browse snapshots** (`tui/browse/view.rs` + `.snap`) -- human-formatted,
  not `--raw`.

## 4. Parser coverage (new test in `cli/src/parse/btrfs_device_usage.rs`)

Add to `mod tests` (synthetic section), matching the file's inline style
(`\x20  ` indent, `RawCommandOutput` literal), with a `//` comment citing
`filesystem-usage.c:820-821`. Two-device stdout: one live, one `missing, ID: 3`
with `Device size: 0`, a preserved `Data,RAID1:` line, and a **nonzero**
`Unallocated`. Assert `device_size == 0`, `devid == 3`, `path == "missing"`,
and the allocation + unallocated values survive. This pins the contract
`check_relocation_space` depends on (missing == `device_size == 0`, allocations
measurable) and proves the opaque-path parse of the real marker.

## 5. Device-stats fidelity: fixtures + tool-behavior docs

Real v6.17.1 missing-device string is `devid:N`. Correct the fixtures:

1. `tui/probe.rs:1467` -- inline JSON `"device": "<missing disk>"` -> `"device": "devid:2"` (keep `"devid": 2` and `read_io_errs: 9`; the test binds by persisted devid, not the string).
2. `tui/probe.rs:1389` -- comment token `<missing disk>` -> `devid:2`.
3-4. `cli/tests/fixtures/nixos-25.11/btrfs-device-stats-degraded.{json,txt}` -- `"device": "devid:2"` / `[devid:2]` (txt: change only the bracketed token on all 5 lines; keep value alignment).
5-6. `cli/tests/fixtures/nixos-unstable/btrfs-device-stats-degraded.{json,txt}` -- same.

The golden test `golden_btrfs_device_stats_degraded`
(`cli/tests/support/golden_common.rs:145-162`) asserts only `devids == [1,2]`
and zero counters -- it never reads the `device` string, so it stays green. The
`.txt` golden is captured-but-unparsed (no raw stats parser); corrected for
consistency only.

### 5.1 Tool-behavior docs sync

The tracked tool-behavior reference still presents `[<missing disk>]` as the
*current* `btrfs device stats` rendering for a missing device, which now
contradicts the corrected fixtures and the verified v6.17.1 source
(`reference/btrfs-progs/cmds/device.c:625-634`). Per AGENTS.md, code/behavior
changes must keep these docs consistent. Update
`docs/internals/tool-behavior/device-disappearance.md` (3 occurrences, not the
2 the finding cited):

- **Line 19** (state table, "Fully gone" row): `btrfs device stats` cell
  `[<missing disk>]` -> `[devid:N]`.
- **Line 43** (prose, "MISSING with path"): example pair
  `(`[/dev/mapper/X]` or `[<missing disk>]`)` -> `(`[/dev/mapper/X]` or
  `[devid:N]`)`. The devid-keying point is unchanged.
- **Line 54** (prose, "Fully gone"): state that pinned btrfs-progs v6.17.1
  renders the missing-device stats path as `devid:N` (so the row reads
  `[devid:N]`), citing `device.c:625-634`; reframe `[<missing disk>]` as an
  *older* btrfs rendering rather than current behavior. Keep the invariant that
  braid ignores the device string and keys on `devid`.

Leave `docs/internals/real-world/sata-hot-unplug.md` as-is (preserve historical
observations): line 87 is an explicit real-hardware observation (hot-unplug
stats kept the mapper path, *not* the sentinel) and line 157 already names both
`<missing disk>` and `devid:<n>` as handled sentinels. Optionally add a one-line
note that the current sentinel is `devid:N`, but do not rewrite the empirical
log.

## 6. Execution order

1. Add `DeviceUsageSpec` + constructors + `device_usage_raw_body` to
   `shared.rs`; re-export from `test_fixtures.rs`. `just test-rust` (compiles;
   transient dead-code warning until wired). Add the **§1.1 builder regression
   test** in this same step so the builder's exact output is pinned before any
   fixture routes through it; it must pass before migration begins.
2. Migrate per module, running `just test-rust` after each: `remove.rs` ->
   `replace.rs` -> `remove_missing.rs` (consts + inline) -> `doctor.rs`
   (consts + `enospc_device_usage` adapter) -> `status.rs` (+ the enospc-risk
   fixture). Switch each `*Pool` usage field `&'static str` -> `String`.
3. Correct the device-stats fixtures and sync the tool-behavior docs
   (Section 5 + §5.1). `just test-rust`.
4. Add the parser test (Section 4). `just test-rust`.

## Verification

- **`just test-rust`** is the full gate (runs `--lib`, `--bin braid`,
  `golden_nixos_25_11`, `tty_guard`, `confirm_yes`). Migrated fixtures are
  exercised by `--lib`; the corrected golden stats by `golden_nixos_25_11`.
- Spot-confirm parse-equivalence: the `remove`/`remove_missing`/`doctor`/
  `status` enospc/capacity assertions and the `replace`/`remove_missing`
  missing-device branches must remain green unchanged (only fixture *source*
  changes, not asserted values).
- No VM run required. Per v6.17.1 source the hand-corrected golden stats match
  what `just capture-all-fixtures` would yield (`devid:N`); a future capture
  would surface a diff only if upstream changes the rendering -- which is the
  desired early-warning behavior.
- Grep guard after implementation: `git grep -n "<missing disk>" -- cli/` must
  return **nothing** (every fixture corrected). Under `docs/`, the only
  remaining `<missing disk>` mentions may be those explicitly framed as
  historical / older-version behavior (`device-disappearance.md` line 54 reframe
  and the two `sata-hot-unplug.md` observations); no doc may present it as the
  *current* `btrfs device stats` rendering.
