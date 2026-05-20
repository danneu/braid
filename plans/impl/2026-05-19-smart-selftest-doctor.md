# `braid doctor`: warn on stale SMART self-test logs

## Context

braid does not schedule SMART short/long self-tests. ADR 016 (auto-suspend)
intentionally keeps smartd opportunistic so it never inhibits suspend or
schedules wake-ups. The two design alternatives to fill the gap are:

- Add a per-disk scheduler (the path TrueNAS Scale just retreated from in
  25.10 and that Unraid never took -- significant config surface, conflicts
  with auto-suspend).
- Surface it in `braid doctor` instead: warn when any pool drive has no
  completed SMART self-test in the last 90 days, with a copy-paste hint.

This plan takes the second path: a single new doctor check, no new config,
no new daemons or timers, no per-drive cron expressions. It fits the "doctor
surfaces, user decides" principle and composes cleanly with the existing
monthly btrfs scrub.

## Behaviour

For each disk in pool membership, run
`smartctl --json -A -l selftest <by-id>` and parse two pieces of
information from the response: the self-test table at
`ata_smart_self_test_log.standard.table[]` plus its companion counters
(`error_count_total`, `error_count_outdated`), and `power_on_time.hours`.

The `-A` flag is required: `power_on_time` is emitted only when vendor
attribute printing is enabled (see `reference/smartmontools/smartmontools/ataprint.cpp:1178`
and the option gate at `ataprint.cpp:4163` -- `smart_vendor_attrib`). Without
`-A`, every drive would skip with "missing power_on_time" and the check
would be inert.

Failure semantics follow smartmontools itself: a failed self-test is
treated as active until a newer successful **extended** self-test
supersedes it (see `ataprint.cpp:2743-2763` and `smartctl.8.in:2519-2522`).
A newer successful short does NOT clear a prior failure. smartctl
already exposes this distinction in JSON as `error_count_total` vs
`error_count_outdated`; the difference is the number of active (unsuperseded)
failures. The plan reuses those fields directly rather than re-deriving them.

Staleness is measured in powered-on hours (the natural unit, since the
self-test log is keyed by lifetime hours) and is wrap-aware: ATA
`lifetime_hours` wraps at 2^16 = 65536 hours / ~7.5 years
(`smartctl.8.in:1453-1456`), while `power_on_time.hours` from attribute 9
is a raw48 counter that does not wrap in practice
(`ataprint.cpp:1178`). The age formula is therefore expressed in modular
arithmetic so the inputs are masked into the same 16-bit window before
subtraction:

```
age_hours = ((power_on_hours % 65536) + 65536 - (entry_lifetime_hours % 65536)) % 65536
```

This is correct for any entry younger than one wrap window (which the
21-entry circular buffer practically guarantees in normal use). The
unavoidable edge case is a drive that ran tests, then ran no tests for
~7.5+ years -- such entries would mod-wrap to a small false-recent age,
and doctor would falsely report `Ok`. v1 accepts that edge as an
inherent limitation of stateless 16-bit-wrapped ATA log timestamps:
without persisting our own observation history (an explicit non-goal
for v1 to keep the check stateless and free of new on-disk state), the
log alone cannot disambiguate "tested 500 hours ago" from "tested
66036 hours ago". btrfs scrub and other braid health signals are
unrelated to self-test cadence and explicitly do NOT cover this edge.

**Each drive produces its own `CheckResult`**, not one aggregate.
The check function returns `Vec<CheckResult>` and `run_doctor`
extends the flat `checks` array; the existing worst-of-N status
aggregator at `cli/src/doctor.rs:955-964` (`Fail > Warn > Ok`,
with `Skip` not affecting overall status -- an all-`Skip` outcome
still returns `Ok`) computes the top-level `DoctorReport.status`
over the flat list and needs no changes.

Each per-drive `CheckResult` carries:

- `name: "smart_self_test"` -- stable JSON key, identical across all
  per-drive rows for this check. Consumers filter by `name` to find
  all SMART self-test results.
- `subject: Some(<member_name>)` -- the pool member name (e.g.,
  `"disk1"`). The JSON-stable disk identity. **Crucially, the disk
  name is NOT encoded into `name`** -- a hypothetical
  `smart_self_test_disk1` shape would create dynamic JSON keys that
  external scripts cannot consume statically. The stable-name +
  `subject` split keeps the schema enumerable.
- `status` and `message` per the decision matrix.

Special row: if `membership::load_membership(ctx.paths)` fails or
returns no drives (no pool.json, or pool not yet enrolled), the
check emits a single unscoped `CheckResult` with
`subject: None`, `status: Skip`, and a message that names the
underlying enumeration failure. This is the only row in this check
that may have `subject: None`. Per-drive rows always have a subject.

The human formatter renders `subject` after the label, joined by a
single space. Example rendered output:

```
[ok]   smart selftest disk1  passed ~2 days ago
[ok]   smart selftest disk2  passed ~12 days ago
[warn] smart selftest disk3  no completed SMART self-test recorded -- run: smartctl -t short /dev/disk/by-id/...
```

Rows for other checks (which do not set `subject`) render exactly as
they do today; the subject-aware rendering only kicks in when
`subject` is `Some`.

Decision matrix (evaluated per drive, in order). Each row produces
the `message` for one per-drive `CheckResult`. The disk identity
lives in `subject`, NOT in the message text -- the message column
below shows the message body only.

| Condition | Per-drive status | `message` body |
|---|---|---|
| `ctx.runner.run(...)` returns `Err(e)` (smartctl could not be spawned -- missing binary, permission denied, transport error) | `Skip` | `SMART self-test status unavailable (smartctl command failed to run: <e>)` |
| `command_error` (bits 0-2 of exit status set) | `Skip` | `SMART self-test status unavailable (smartctl command failed)` |
| `parse_failure` (JSON unparseable) | `Skip` | `SMART self-test status unavailable (smartctl JSON output not parseable)` |
| `unsupported_protocol` is `Some(p)` (drive is non-ATA: NVMe / SCSI / unknown) | `Skip` | `SMART self-test status unavailable (<p> self-test log not checked in v1)` |
| `active_errors > 0` AND `last_failure` populated | `Fail` (detail) | `SMART self-test FAILED at lifetime hour <H> (<type>) -- investigate before further use` |
| `active_errors > 0` AND `last_failure` is `None` | `Fail` (fallback) | `SMART self-test log reports <N> active failure(s) but no failure entry was parsed -- run smartctl manually: smartctl -l selftest <by-id>` |
| `power_on_hours.is_none()` (cannot measure age) | `Skip` | `SMART self-test status unavailable (power_on_time.hours missing -- can't measure age)` |
| No active failure AND `last_passing` populated AND age within `STALE_SELFTEST_THRESHOLD_HOURS` (= 2160 powered-on hours = 90 days) | `Ok` | `passed <age_phrase> ago` |
| No active failure AND `last_passing` populated AND age > `STALE_SELFTEST_THRESHOLD_HOURS` | `Warn` (stale) | `no SMART self-test in <age_phrase> -- run: smartctl -t short <by-id>  (or -t long for full-surface scan, takes hours)` |
| No active failure AND `last_passing` is `None` | `Warn` (never) | `no completed SMART self-test recorded -- run: smartctl -t short <by-id>  (or -t long for full-surface scan, takes hours)` |

The `<age_phrase>` substitution is rendered by a small helper
`approx_days_phrase(age_hours: u64)` that takes integer-truncated
days and pluralises grammatically. Defined under the
`cli/src/doctor.rs` section below. Rendered shapes:

- 47 hours -> `~1 day` (truncates to 1; singular)
- 48 hours -> `~2 days`
- 24 hours -> `~1 day`
- 5 hours -> `~0 days` (zero takes plural in English; appears on
  real hosts whenever a self-test has just completed -- the
  manual-smoke step in Verification below explicitly hits this
  path. Acceptable because the check intentionally reports coarse
  powered-on-day age, and the `~` qualifier flags the imprecision.)
- 3000 hours -> `~125 days`

Truncation rule: `days = age_hours / 24` (Rust `u64` division).
No additional rounding logic. The leading `~` carries the implied
imprecision. The singular/plural split matches the project's
existing user-facing convention -- see `cli/src/doctor.rs:707-708`
(foreign LUKS UUID `plural = if n == 1 { "" } else { "s" }`),
`cli/src/mount.rs:1454-1475` (pinned tests for
"1 missing device" vs "N missing devices"), and
`cli/src/preflight.rs:849-853`.

Gate ordering rationale. The runner-error row sits first because
without an `Ok(RawCommandOutput)` value there is no `exit_status`,
no stdout, and no parsed summary to interrogate at all -- every
downstream field is structurally inaccessible. Then the three
"parser couldn't trust the input" Skips (`command_error`,
`parse_failure`, `unsupported_protocol`) come next because they
invalidate every downstream field. Then the two
`Fail` rows fire on `active_errors > 0` -- crucially BEFORE the
missing-POH gate, because `active_errors` is computed purely from
`error_count_total - error_count_outdated` and does not depend on
power_on_hours. A drive that reports an active SMART failure but
elides attribute 9 (rare in practice -- some drives censor POH;
some smartctl invocations strip it) must surface as `Fail`, not
`Skip`. Only the age computation needs POH, so the missing-POH gate
sits between Fail and Ok/Warn: it suppresses the age-comparison
branches but does not suppress failure detection.

The two `Fail` rows split on whether the table walk surfaced a
Failed-classified entry: in correct smartctl output a non-zero
`active_errors` count always comes with a corresponding entry, but
the fallback row defends against parser drift, malformed-but-parseable
JSON, and any future smartctl shape change that decouples the counters
from the table -- without it the detail row's `<H>` and `<type>`
substitutions would have nothing to render. The two Warn rows split on
`last_passing.is_none()`: the stale row carries the computed age in
days, the never row does not (no completed passing entry exists, so
no age is defined). Both emit the same copy-paste hint. The never-row
covers three real shapes that all reach it: a truly empty log (only
`count: 0`), a non-empty log whose entries are all aborted/in-progress,
and any other state where no entry classifies as `Passed`.

Exit-status handling. smartctl sets bit 7 (= 0x80) when the self-test
log contains active errors (`smartctl.8.in:2485-2522`); bits 0-2 mean
"command-line parse / device open / SMART command failed". The new
parser MUST treat these two classes differently:

- Bit 7 with non-empty stdout: parse normally and surface
  `active_errors > 0`. This is the failed-self-test happy path.
- Bits 0-2 set (regardless of stdout): the JSON, if any, is unsafe to
  interpret; the parser sets `command_error: true` on the summary so the
  doctor can short-circuit to Skip before reading any other field.

This is a tightening of the existing `parse_smartctl` guard
(`cli/src/parse/smartctl.rs:77`), which only short-circuits bits 0-2
when stdout is empty. Reusing that guard verbatim would let a bit-2
response with non-empty stdout silently fall through to a parsed
summary, and the doctor would misclassify it -- the new
`command_error` flag closes that gap.

Self-test entry status interpretation. The JSON `status.passed`
boolean is not emitted uniformly: `ataprint.cpp:2690-2693` omits it
for status codes 0x1 (aborted by host), 0x2 (interrupted), and 0x3
(fatal or unknown error). But `ataprint.cpp:2618-2628` shows status
codes 0x3 through 0x8 ALL count as self-test failures in smartmontools
(they increment `errcnt` / contribute to `error_count_total`). So a
parser that walks for `status.passed == false` would miss case 0x3 --
an active "Fatal or unknown error" entry would be invisible despite
`active_errors > 0`. The parser therefore classifies on `status.value`:

| `status.value >> 4` | Classification          |
|---|---|
| `0x0`               | Passed (completed without error) |
| `0x1`, `0x2`        | Aborted (neither pass nor fail; skipped in walks) |
| `0x3` ..= `0x8`     | Failed (used for `last_failure`) |
| `0x9` ..= `0xE`     | Unknown (skipped) -- reserved by ATA; smartmontools defaults these to the implicit-pass JSON branch at `ataprint.cpp:2692`, but braid treats them conservatively as Unknown since these codes have no defined ATA semantic and should never appear on a real drive. |
| `0xf`               | In progress (skipped in walks) |
| other               | Unknown (skipped) |

The table is reverse-chronological so `table[0]` is most recent
(`ataprint.cpp:2726`). The parser iterates forward and picks the
first `Passed` entry for `last_passing` and the first `Failed` entry
for `last_failure`.

NVMe is out of scope for v1: skip with reason `"NVMe self-test log not
checked in v1"`. ADR 015 confirms braid is HDD-focused, and
`nvme_self_test_log` has a different schema that deserves its own fixture
work.

## Files to modify

### `cli/src/cmd.rs`

Add `SmartctlSelftestLogJson { device: String }` next to the existing
`SmartctlSelftestLog` variant at line 298. Emit
`smartctl --json -A -l selftest <device>` in the arg-generation match at
line 1067. Add the variant to the roundtrip test at line 1779 with the
expected argv `["--json", "-A", "-l", "selftest", "/dev/disk/by-id/disk1"]`.

Leave the existing text-mode `SmartctlSelftestLog` intact -- it is consumed
by the TUI browser (`cli/src/tui/browse/state.rs:1182`) which renders raw
text.

### `cli/src/parse/mod.rs`

The module-level doc comment at `cli/src/parse/mod.rs:1-17` currently
requires that "synthetic scenarios (variant happy-paths,
negative/malformed inputs) must be inline string literals in tests"
(line 15-16). The self-test fixtures introduced by this plan are an
intentional exception: VM virtio disks do not produce useful
`smartctl -l selftest` output, so the fixtures cannot be captured by
`just capture-all-fixtures` and must be hand-authored. Update the doc
comment to record this exception as a single bullet so future readers
do not interpret the new fixtures as a violation:

```text
- Exception: smartctl self-test log fixtures
  (`smartctl-selftest-*.json`) are hand-authored under
  `tests/fixtures/nixos-25.11/` because the NixOS VM toolchain cannot
  produce useful self-test logs on virtio disks. They are
  parser-critical contracts and follow the same review-on-bump
  obligation as captured fixtures (see `AGENTS.md` Parser
  Compatibility).
```

This is the only parser fixture-policy documentation change; the
fixture-policy prose itself lives in the fixture README and
`AGENTS.md` as already planned, and the user-facing manual update
lives in `manual/commands/doctor.md` (see below).

### `cli/src/parse/smartctl.rs`

Add `pub fn parse_smartctl_selftest_log(raw: &RawCommandOutput) -> SelftestSummary`.

Doc-comment intent: a `///` comment on the function explains that the
exit-status policy is intentionally stricter than the existing
`parse_smartctl`: `parse_smartctl` (`cli/src/parse/smartctl.rs:77`)
falls through to JSON parsing on bits 0-2 if stdout is non-empty,
because the worst case there is a misclassified
`SmartHealth::Unknown`. Self-test classification is more brittle --
mis-parsing the active-failure counters could escalate a non-failure
to a `Fail`, or hide a real failure as Skip -- so this parser
short-circuits on any bit-0-2 exit and surfaces it as `command_error`.
This drift between the two functions is intentional; the rationale is
captured in the doc-comment so a future audit does not mistake it for
inconsistency.

Exit-status classification (before any JSON parsing):

- `(exit_status & 0x07) != 0` -> set `command_error: true` on the
  returned summary and skip JSON interpretation entirely. Doctor will
  Skip on this flag.
- Otherwise (including `exit_status == 128` for self-test errors)
  attempt JSON parse.

Bad JSON -> empty summary with `parse_failure: true`.

Protocol detection (after a successful JSON parse, before reading the
ATA log):

- Read `device.protocol` (already extracted by `parse_smartctl` at
  `cli/src/parse/smartctl.rs:36-129`). Branch on the value:
  - ATA / SATA (case-insensitive match) -> proceed to ATA log parse.
  - NVMe -> `unsupported_protocol: Some("NVMe")`.
  - Any other string (e.g., SCSI) -> `unsupported_protocol: Some(<raw string>)`.
  - Missing (`device` absent, or `device.protocol` absent) ->
    `unsupported_protocol: Some("unknown")`. The literal string
    `"unknown"` is used so the Skip reason is never an empty value
    and the doctor message reads `"unknown self-test log not checked
    in v1"`. This intentionally diverges from
    `parse_smartctl`'s default-to-SATA behaviour
    (`cli/src/parse/smartctl.rs:129`): self-test classification is
    brittle enough that a missing-protocol response is treated as
    "not safe to interpret as ATA", whereas the health classifier
    can fall back to SATA because its worst case is a
    `SmartHealth::Unknown`. The trade-off is occasional gratuitous
    Skips on partial JSON in exchange for never misinterpreting a
    non-ATA response.
- On any `unsupported_protocol` result, the parser returns without
  reading `ata_smart_self_test_log.standard`; doctor Skips on the flag.
- Background: NVMe emits `power_on_time.hours` at the same JSON path
  (`nvmeprint.cpp:527`) but the self-test log lives at
  `nvme_self_test_log` with a different schema (`nvmeprint.cpp:663-689`).
  Without this gate, NVMe output would parse `power_on_hours: Some(..)`
  and an empty ATA `last_passing`, then Warn as "no SMART self-test"
  instead of Skipping with the NVMe reason promised in the matrix.

The parser extracts (ATA path):

- `power_on_time.hours` -> `power_on_hours: Option<u64>`.
- `ata_smart_self_test_log.standard.error_count_total: u32`
  (defaults to 0 if absent) and `error_count_outdated: u32` (defaults
  to 0 if absent). The difference is computed with
  `error_count_total.saturating_sub(error_count_outdated)` so a
  malformed input with `outdated > total` clamps to 0 active failures
  instead of underflowing to a huge u32 (wrap in release, panic in
  debug). Both fields are
  absent in the real "no self-tests ever logged" output -- smartmontools
  short-circuits at `ataprint.cpp:2714-2718`, emitting only
  `ata_smart_self_test_log.standard.count = 0` -- so the parser MUST
  treat their absence as zeros, not as malformed input.
- `ata_smart_self_test_log.standard.table[]` (defaults to empty if
  absent; same `count == 0` short-circuit). Walk in array order
  (smartctl emits reverse-chronological per `ataprint.cpp:2726`). For
  each entry, read `type.{value, string}`, `status.value`, and
  `lifetime_hours`. Classify the entry by `status.value >> 4` per the
  table above. Walk forward and surface:
  - `last_passing: Option<SelftestEntry>` -- first entry whose status
    classifies as `Passed`.
  - `last_failure: Option<SelftestEntry>` -- first entry whose status
    classifies as `Failed`. Does NOT depend on the JSON `passed` field
    (so case 0x3 "Fatal or unknown error", which omits `passed`, is
    surfaced correctly).

In Rust, all four ATA-log fields (`count`, `table`, `error_count_total`,
`error_count_outdated`) are deserialized with `#[serde(default)]` so a
truly empty log (only `count: 0` present) parses cleanly into an empty
table with zero active errors.

Age computation helper (private):

```rust
/// Age in powered-on hours, wrap-aware for ATA self-test entries.
/// ATA `lifetime_hours` in the self-test log wraps at 2^16
/// (smartctl.8.in:1453); `power_on_hours` from attribute 9 does not
/// (ataprint.cpp:1178). Mask both to the same 16-bit window before
/// subtracting, then add 65536 and mod again to handle the wrap.
fn selftest_age_hours(power_on_hours: u64, entry_lifetime_hours: u32) -> u64 {
    let poh_mod = power_on_hours % 65536;
    let entry_mod = (entry_lifetime_hours as u64) % 65536;
    (poh_mod + 65536 - entry_mod) % 65536
}
```

### `cli/src/parse/types.rs`

Add:

- `SelftestSummary { command_error: bool, parse_failure: bool, unsupported_protocol: Option<String>, power_on_hours: Option<u64>, active_errors: u32, last_passing: Option<SelftestEntry>, last_failure: Option<SelftestEntry> }`.
- `SelftestEntry { kind: SelftestKind, lifetime_hours: u32, status_value: u8, status_string: String }`.
- `SelftestKind { Short, Extended, Conveyance, Selective, Offline, Other(String) }`.
- `SelftestStatusClass { Passed, Aborted, Failed, InProgress, Unknown }`
  -- declared `pub(crate)` only if the doctor needs it directly;
  the planned doctor tests assert on message text and `last_passing` /
  `last_failure` populated-ness rather than on a status class, so the
  default is to keep this enum private to `cli/src/parse/smartctl.rs`
  and NOT add it to `cli/src/parse/types.rs`.

`unsupported_protocol` carries the verbatim protocol string from
`device.protocol` so the Skip message can quote what smartctl reported
(e.g., `"NVMe self-test log not checked in v1"` vs.
`"SCSI self-test log not checked in v1"`).

Place next to `SmartProbe` / `SmartHealth`. `SelftestKind::Other`
carries the verbatim `type.string` so the Fail message can quote what
smartctl reported even for vendor/reserved test types. `SelftestEntry`
omits a derived `passed: bool` -- callers classify on `status_value`
via the table in the Behaviour section so case 0x3 is not lost.

### `cli/src/doctor.rs`

**Add a `subject` field to `CheckResult`.** Extend the struct at
`cli/src/doctor.rs:47-51` with:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckResult {
    pub name: String,
    pub status: CheckStatus,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
}
```

The `PartialEq, Eq` additions are required by the planned
`json_roundtrip_preserves_subject` test (under Tests below), which
asserts equality between an original `CheckResult` and the
round-tripped value. `CheckStatus` already derives `PartialEq, Eq`
at `cli/src/doctor.rs:37`, and `String` / `Option<String>` derive
them natively, so adding them to `CheckResult` is a single
attribute-list edit with no downstream implications.

The `skip_serializing_if` attribute keeps the JSON shape backwards
unchanged for the existing checks (none set `subject`, so the field
is OMITTED from JSON, not rendered as `null`). This has two
consequences that consumers and tests must account for:

- Per-drive `smart_self_test` rows always have `subject: Some(<disk>)`,
  so the JSON field is always present as a non-empty string.
- The single unscoped membership-failure row has `subject: None`, so
  the JSON field is ABSENT (omitted by `skip_serializing_if`). It is
  not present-as-`null`.

The serialization asymmetry is intentional: it preserves the existing
checks' JSON shape verbatim (no `"subject": null` noise on every
existing row), at the cost of requiring consumers to treat
"`subject` field present and non-empty" as the only positive
disambiguator. The plan tests and VM-test assertions are written to
match this contract.

**Update every existing direct `CheckResult` struct literal in
`cli/src/doctor.rs` to include `subject: None`.** Rust struct literals
require every field; the new `subject` field would otherwise fail to
compile. `grep -n "CheckResult {" cli/src/doctor.rs` against the
working tree shows **13 such literal sites, all inside
`#[cfg(test)] mod tests` (which starts at line 1147)**, on lines
1501, 1506, 1515, 1520, 1532, 1537, 1546, 1551, 1564, 1625, 1630,
1635, and 1640. Production-code paths -- including the `check_*` /
`summarize_*` helpers -- construct `CheckResult` exclusively via the
`ok/warn/fail/skip` constructor helpers, so updating those helper
bodies (which absorb the new field internally) covers every
production caller in one place. No other file in `cli/src/`
constructs `CheckResult` directly. The mechanical edit at each test
literal site is to append `subject: None,` after the existing
`message: ...,` line.

**Update the existing `valid_config_parses_ok_declared_disks_skips`
test at `cli/src/doctor.rs:1267-1289`.** Its assertion
`assert_eq!(report.checks.len(), 11);` at line 1278 pins the exact
number of `CheckResult` rows produced by `run_doctor` in a scenario
with a valid config but no pool.json. The new `smart_self_test`
check is wired into `run_doctor` and, in this scenario, emits the
single unscoped `Skip` row (membership cannot be enumerated because
`isolated_paths()` provides no pool.json), so the row count
deterministically increases by exactly one. The edit:

- Change line 1278 to `assert_eq!(report.checks.len(), 12);`.
- Add directly below it:

  ```rust
  let selftest = find_check(&report, "smart_self_test");
  assert_eq!(selftest.status, CheckStatus::Skip);
  assert_eq!(selftest.subject, None);
  ```

This pins both the row-count change AND the unscoped-Skip
behaviour (`subject: None`) in the same scenario. `find_check`
already exists at `cli/src/doctor.rs:1162-1168` and returns the
first row with the matching `name`; for this check that's the
single unscoped row. No other `report.checks.len()` assertion
exists in `cli/src/doctor.rs` (verified with
`grep -n "checks.len()" cli/src/doctor.rs`), so this is the only
count assertion that needs updating in production / inline-test
code.

The existing helper constructors (`CheckResult::ok/warn/fail/skip`
at lines 54-84) keep their two-argument signatures and default
`subject: None`. Add four parallel constructors that take a subject:

```rust
fn ok_for(name, subject, message) -> Self { ... }
fn warn_for(name, subject, message) -> Self { ... }
fn fail_for(name, subject, message) -> Self { ... }
fn skip_for(name, subject, message) -> Self { ... }
```

Only the new SMART self-test check uses `*_for`; other checks
continue to use the existing two-argument helpers unchanged.

**Update the human formatter** at
`cli/src/doctor.rs:1012-1045`. When `c.subject` is `Some(s)`, render
the label + subject together (joined by a single space) in place of
the bare label inside the `{label:<14}` column. The simplest
approach: build a `display_label` String once per row -- if
`subject.is_none()`, it's the existing matched label; otherwise it's
`format!("{label} {subject}", ...)` -- and feed THAT into the
existing `{label:<14}` formatting. The padding gracefully expands
when the combined string exceeds 14 characters (Rust's `<14` is a
minimum width, not a cap), and shorter labels still align with
existing rows. The user-visible result matches the example in the
Behaviour section above:

```
[ok]   smart selftest disk1  passed ~2 days ago
[warn] smart selftest disk2  no completed SMART self-test recorded -- run: smartctl -t short /dev/disk/by-id/...
```

**Add the new check function:**

```rust
fn check_smart_selftests<R: CommandRunner>(
    ctx: &mut DoctorContext<'_, R>,
) -> Vec<CheckResult>
```

Note the function name (`check_smart_selftests`, plural) and the
return type (`Vec<CheckResult>`, not a single `CheckResult`).

Membership-enumeration branch (runs before any smartctl invocation):

- Call `membership::load_membership(ctx.paths)`. If it errors, OR if
  it returns an empty iterator (no pool members), return a
  single-element `Vec` containing one `CheckResult::skip` with
  `name = "smart_self_test"`, `subject = None`, and a message that
  names the failure mode (e.g., `"pool membership not enumerable
  (<error>)"` or `"no pool members declared"`). This is the only
  shape in which this check emits a row with `subject: None`.

Per-drive branch (one CheckResult per `(uuid, member)` pair from
`iter_by_name()`):

- Call `ctx.runner.run(&CmdRequest::SmartctlSelftestLogJson { device: member.by_id.as_str().into() })`.
  Match the `Result`. Walk these gates in this exact order to produce one
  `CheckResult` for this drive with
  `subject = Some(member.name.as_str().into())` and message body
  from the matching matrix row:

  0. `Err(e)` -> `Skip`. The smartctl process could not be spawned
     (missing binary, permission denied, transport error). Embed
     `e.to_string()` (which `CmdError`'s `Display` impl already
     renders -- see `cli/src/cmd.rs:1189-1194`) into the message
     body via the runner-error row template. This mirrors the
     established `match runner.run(...) { Ok(raw) => ..., Err(e) => ... }`
     pattern at `cli/src/doctor.rs:302-307` and `:308-311`.
     Subsequent gates only apply to the `Ok(raw)` branch, where
     `parse_smartctl_selftest_log(&raw)` produces a `SelftestSummary`
     to interrogate.
  1. `summary.command_error` -> `Skip`.
  2. `summary.parse_failure` -> `Skip`.
  3. `summary.unsupported_protocol` is `Some(p)` -> `Skip`.
  4. `summary.active_errors > 0` -> `Fail`. **This gate fires BEFORE
     the missing-POH check** because `active_errors` comes from
     `error_count_total - error_count_outdated` and is independent
     of POH. A drive with a real failure must not be silenced by a
     missing attribute-9 emission. Use the detail row's message
     when `last_failure` is `Some`, the fallback row's message
     otherwise.
  5. `summary.power_on_hours.is_none()` -> `Skip`.
  6. `last_passing` is `Some` AND age <= `STALE_SELFTEST_THRESHOLD_HOURS` -> `Ok`.
  7. `last_passing` is `Some` AND age > `STALE_SELFTEST_THRESHOLD_HOURS` -> `Warn`
     (stale, with age).
  8. `last_passing` is `None` -> `Warn` (never).

Declare the threshold as a module-level constant in
`cli/src/doctor.rs` next to the new check function so the decision
matrix, the gate expressions, and the manual all reference the same
source of truth:

```rust
/// SMART self-test staleness threshold in powered-on hours.
/// 90 days at 24 h/day. Matches the manual's "90 powered-on days"
/// wording and the decision matrix in the plan.
const STALE_SELFTEST_THRESHOLD_HOURS: u64 = 90 * 24;
```

Declare the `<age_phrase>` formatter as a module-private helper in
the same file, used by both the Ok ("passed ...") and the stale
Warn ("no SMART self-test in ...") branches of the per-drive
matrix. The singular case renders `~1 day`; every other count
renders `~N days`. Integer truncation is via `u64` division so
borderline cases (e.g., 47 h -> 1 day) match the substring
assertions in the tests:

```rust
/// User-facing age phrase for SMART self-test messages.
/// Truncates to whole days and pluralises grammatically; matches
/// the project convention of pinning singular/plural wording in
/// CLI output (see foreign-LUKS-UUID and missing-device prose).
fn approx_days_phrase(age_hours: u64) -> String {
    let days = age_hours / 24;
    if days == 1 {
        "~1 day".to_owned()
    } else {
        format!("~{days} days")
    }
}
```

  Push each per-drive `CheckResult` into the returned `Vec`. Order
  matches `iter_by_name()` so the same membership order shows up
  consistently in the doctor output.

Step 0 short-circuits on spawn failure where no other field is
even structurally accessible. Steps 1-3 invalidate every downstream
field; step 4 short-circuits on failure detection before any age
math; step 5 is the narrowest Skip because it only suppresses the
age-comparison branches.

**Wire into `run_doctor`** (line 979-991). The existing checks vec
collects per-check `CheckResult`s with `push`; the new check
returns `Vec<CheckResult>` and is appended with `extend`. The
top-level `DoctorReport.status` aggregator (`worst-of-N` over the
flat `checks` array) already handles multiple entries from a single
check; no aggregator changes needed.

**Label map entry** at `cli/src/doctor.rs:1021-1037`:

```rust
"smart_self_test" => "smart selftest",
```

The 14-character label fits the existing `{label:<14}` minimum-width
column at line 1041 exactly for unscoped rows. Subject-bearing rows
extend the column naturally because Rust's `<14` is a floor, not a
cap (see formatter notes above). The JSON shape stays as the
snake_case key per the project's existing convention (see
`config_file`, `declared_disks`, `pool_missing_devices`,
`foreign_luks_uuid` at the same site).

### `manual/commands/doctor.md`

Update the published end-user guide for `braid doctor` so it stays in
sync with the new check. Three coordinated edits in this file:

1. **Sample output (lines 22-30):** add one `smart selftest` line
   per pool drive (the check emits one `CheckResult` per drive --
   see Behaviour above). Place them as a group after `meta profiles`
   and before `alert beep` so the order matches the
   disk-health-then-hardware grouping. The sample uses three drives,
   all `Ok`:

   ```
   [ok]   smart selftest disk1  passed ~2 days ago
   [ok]   smart selftest disk2  passed ~12 days ago
   [ok]   smart selftest disk3  passed ~30 days ago
   ```

   Add a short note immediately after the sample block explaining
   that this check emits one row per pool drive (so users aren't
   surprised by N rows for an N-drive pool), and show a mixed
   example below so users see what a problem looks like, e.g.:

   ```
   [warn] smart selftest disk2  no completed SMART self-test recorded -- run: smartctl -t short /dev/disk/by-id/...
   ```

2. **Check table (lines 50-61):** add a `smart_self_test` row.
   Mention the per-drive emission shape so JSON consumers know what
   to expect:

   | Check | What it does |
   | --- | --- |
   | `smart_self_test` | One result per pool drive: runs `smartctl --json -A -l selftest <by-id>` against each, then reports `Fail` on an active SMART self-test failure, `Warn` if no completed test in the last 90 powered-on days (or never), `Ok` otherwise, or `Skip` for NVMe/SCSI/unsupported drives. In `--json`, every per-drive result carries `name: "smart_self_test"` and a `subject` field naming the pool member; if pool membership cannot be enumerated, a single `Skip` result with `name: "smart_self_test"` is emitted and the `subject` field is omitted. Scripts should check whether `subject` is present before keying on it. |

3. **What happens under the hood (lines 76-83):** insert a new step
   describing the smartctl invocation. Suggested placement after the
   missing-devices probe (so it stays grouped with the disk-health
   sequence) and before the `--beep` step:

   ```
   8. For each declared disk, runs `smartctl --json -A -l selftest <by-id>` and parses the self-test log to detect active failures and report the age of the most recent passing entry.
   ```

   Renumber the subsequent steps.

The file's "Related commands" and "Flags" tables do not need changes;
the new check adds no flags and is read-only like the rest of doctor.

### `cli/src/test_fixtures/doctor.rs`

Extend the existing mock-runner helpers to dispatch
`CmdRequest::SmartctlSelftestLogJson` to fixtures. Keep the existing
patterns (`MockRunner`, `parsed_doctor_ctx`, etc.) -- no new abstraction.

### `cli/tests/fixtures/nixos-25.11/`

Add hand-crafted JSON fixtures (VM virtio disks don't emit useful self-test
logs, so `just capture-all-fixtures` cannot produce these). Files are
strict JSON (no comments -- JSON doesn't allow them):

- `smartctl-selftest-ata-recent-pass.json` -- short pass ~50 hours old,
  `error_count_total = 0`, `error_count_outdated = 0`.
- `smartctl-selftest-ata-stale.json` -- short pass ~3000 hours old,
  no failures.
- `smartctl-selftest-ata-active-failure.json` -- failed extended,
  no superseding extended pass, `error_count_total = 1`,
  `error_count_outdated = 0`. Paired with `exit_status = 128` in the
  parser test to match smartctl's real-world behaviour
  (`smartctl.8.in:2519`).
- `smartctl-selftest-ata-failure-outdated.json` -- failed short followed
  by a newer passing extended at `lifetime_hours` close to current
  `power_on_time.hours` (the superseding pass is recent, ~50 hours
  old). `error_count_total = 1`, `error_count_outdated = 1`
  (active errors = 0). Drives the "outdated failures are NOT a Fail"
  case with a deterministic `Ok` outcome -- the recent superseding
  pass ensures the doctor never lands on the stale-Warn branch.
- `smartctl-selftest-ata-short-pass-does-not-supersede.json` -- failed
  extended followed by passing short only (no extended pass in between),
  so the failure is still active. `error_count_total = 1`,
  `error_count_outdated = 0`. Asserts a passing short does NOT clear
  the failure. (Renamed from the previous misleading
  `recent-pass-supersedes` shape: the fixture demonstrates the
  *opposite* -- that a short pass does NOT supersede a prior failure
  -- so the filename now matches its invariant.)
- `smartctl-selftest-ata-empty.json` -- truly empty log, matching
  smartmontools's real shape at `ataprint.cpp:2714-2718`:
  `ata_smart_self_test_log.standard` contains ONLY `revision` and
  `count: 0`. No `table`, no `error_count_total`, no
  `error_count_outdated`. Drives the `#[serde(default)]` invariant.
- `smartctl-selftest-ata-aborted-only.json` -- non-empty `table[]`
  containing only entries with `status.value >> 4 == 0x1` (Aborted by
  host) and/or `0x2` (Interrupted). `count > 0`, `error_count_total = 0`.
  Drives the "non-empty log but no passing completed test" Warn path.
- `smartctl-selftest-nvme-unsupported.json` -- `device.protocol = "NVMe"`,
  `power_on_time.hours` populated, no `ata_smart_self_test_log`. Drives
  the unsupported-protocol Skip.
- `smartctl-selftest-ata-wrap-window.json` --
  `power_on_time.hours = 70000`, recent pass at
  `lifetime_hours = 3964`. After wrap-aware math the true age is 500
  hours: `(70000 % 65536 + 65536 - 3964) % 65536 = (4464 + 65536 - 3964) % 65536 = 500`.
  Drives `selftest_age_hours`'s mod-65536 path.
- `smartctl-selftest-ata-fatal-or-unknown.json` -- status value
  `0x3X` (Fatal or unknown error), `passed` field absent in the JSON,
  `error_count_total = 1`, `error_count_outdated = 0`. Drives the
  status-value classifier (case 0x3 must surface as `Failed` even
  without `status.passed`).
- `smartctl-selftest-ata-command-error.json` -- bit 2 of exit status
  set (e.g., `exit_status = 4`) with NON-EMPTY stdout (a parseable
  but unsafe-to-trust JSON response). Drives the `command_error`
  short-circuit so the doctor Skips before reading the body.

Smartmontools is parser-critical per `AGENTS.md` (the "Parser
Compatibility" section). Document the selftest fixture policy in the
existing fixture README (`cli/tests/fixtures/nixos-25.11/README.md`):
these fixtures are hand-authored (the capture script does not
regenerate them), but they MUST be reviewed on smartmontools or
nixpkgs bumps, identical to the other parser-critical fixtures. A
parser-critical bump that changes the `ata_smart_self_test_log.standard`
JSON shape is a required-refresh event for these fixtures.

## Tests

### Parser unit tests (`cli/src/parse/smartctl.rs#[cfg(test)]`)

Follow the existing TDD preamble style (Intent / Why it exists / Scenario,
per `docs/testing.md`). New tests:

- `selftest_recent_pass_parsed`: ~50-hour-old short pass parses,
  surfaces `last_passing.lifetime_hours`, `active_errors == 0`.
- `selftest_active_failure_with_exit_128`: fixture with one failed
  extended (status `0x5X` "electrical failure") and no superseding
  extended pass; runner returns `exit_status = 128` and non-empty
  stdout. Parser MUST still parse and report `active_errors == 1`
  and a populated `last_failure`. Pins the bit-7 exit-status guard.
- `selftest_command_error_bit_2_does_not_parse`: runner returns
  `exit_status = 4` with non-empty stdout (parseable JSON).
  `command_error: true`, all other fields default/empty. Pins the
  tightened command-error gate.
- `selftest_fatal_or_unknown_classified_as_failed`: entry with
  `status.value = 0x30` (Fatal or unknown error) and NO `status.passed`
  field in JSON. Parser MUST surface this as `last_failure` (case 0x3).
  Pins the status-value classifier.
- `selftest_aborted_not_failed`: entry with `status.value = 0x10`
  (Aborted by host). Not surfaced as `last_passing` or `last_failure`.
- `selftest_outdated_failure_not_active`: failed short followed by
  passing extended; `error_count_total == 1`,
  `error_count_outdated == 1`, so `active_errors == 0`.
- `selftest_short_pass_does_not_supersede_failure`: failed extended
  followed only by a passing short; `error_count_outdated == 0`,
  `active_errors == 1`. Pins the smartmontools semantic that only an
  extended pass clears a failure.
- `selftest_empty_log_real_shape`: fixture has only `revision` and
  `count: 0` under `ata_smart_self_test_log.standard`. Parser MUST
  produce `last_passing: None`, `active_errors: 0` via `serde(default)`,
  NOT `parse_failure: true`. Pins the real-world empty-log shape from
  `ataprint.cpp:2714-2718`.
- `selftest_aborted_only_no_passing`: non-empty `table[]` with only
  aborted/interrupted entries; `last_passing: None`, `last_failure: None`,
  `active_errors: 0`. Pins the "table has entries but classifier
  yields no Passed" path.
- `selftest_nvme_unsupported_protocol`: `device.protocol = "NVMe"`;
  `unsupported_protocol: Some("NVMe")`, ATA fields left at defaults.
- `selftest_scsi_unsupported_protocol`: `device.protocol = "SCSI"`;
  `unsupported_protocol: Some("SCSI")`. Pins that the gate is NOT a
  hardcoded "is it NVMe?" check.
- `selftest_missing_protocol_unsupported`: `device.protocol` absent
  (test with both shapes: `device` block missing entirely, and
  `device` present but no `protocol` field).
  `unsupported_protocol: Some("unknown")` exactly. Pins that the new
  parser does NOT default missing protocol to SATA the way
  `parse_smartctl`'s health classifier does at
  `cli/src/parse/smartctl.rs:129`, and pins the literal placeholder
  string so the Skip message is deterministic.
- `selftest_malformed_outdated_exceeds_total`: inline JSON literal
  (per `parse/mod.rs`'s synthetic-scenario rule -- no on-disk fixture
  needed) with `error_count_total: 1` and `error_count_outdated: 5`.
  Asserts `active_errors == 0` via `saturating_sub`, NOT a wrapped
  value. Pins the defensive guard.
- `selftest_active_errors_without_failure_entry`: counters say
  `error_count_total = 1`, `error_count_outdated = 0` (so
  `active_errors = 1`), but `table[]` contains only entries whose
  status classifier yields something other than `Failed` (e.g., a
  fixture with only Aborted entries -- defensively malformed for
  smartctl, but possible after parser drift). Parser MUST surface
  `active_errors = 1` and `last_failure: None` without panicking.
- `selftest_no_power_on_time`: missing `power_on_time.hours` ->
  `power_on_hours: None`; doctor will route to Skip.
- `selftest_bad_json`: garbage stdout -> empty summary,
  `parse_failure: true`.
- `selftest_age_wraps`: `power_on_hours = 70000`,
  `entry.lifetime_hours = 3964` (true age 500 h); assert the helper
  returns 500. Add an assertion for an unwrapped case
  (`power_on_hours = 70000`, `entry.lifetime_hours = 4464`) returning 0,
  and a "way past one wrap" case (entry from ~1 hour after a wrap;
  ensure the formula picks the most recent valid interpretation).
- `selftest_table_is_reverse_chronological`: multiple entries; the
  `last_passing` chosen is the first passing entry walking forward
  from `table[0]`, not the one with the largest `lifetime_hours`.

### Doctor unit tests (`cli/src/doctor.rs#[cfg(test)]`)

The check function returns `Vec<CheckResult>`, so each test asserts
on the returned vector. Pattern: helper `selftest_results_for(...)`
constructs the context, runs `check_smart_selftests(&mut ctx)`, and
returns the `Vec`. Per-drive assertions filter by `subject`:

```rust
fn by_subject<'a>(results: &'a [CheckResult], subject: &str) -> &'a CheckResult {
    results.iter()
        .find(|r| r.subject.as_deref() == Some(subject))
        .unwrap_or_else(|| panic!("no result for subject {subject}"))
}
```

Per-drive classification tests (each uses a single-drive pool so the
test stays focused; the assertion form is "the one returned
`CheckResult` has subject=Some(drive_name), and the expected status
and message body"):

- `check_smart_selftest_recent_pass` -> single `Ok` result. Subject
  matches the drive name. Uses the recent-pass fixture (last passing
  entry ~50 hours old, truncating to 2 days), so the assertion is on
  the literal substring `passed ~2 days ago` (plural form via
  `approx_days_phrase`). The singular-day boundary has its own
  dedicated test below.
- `check_smart_selftest_active_failure_exit_128` -> single `Fail`.
  Mock runner returns `exit_status = 128` and the active-failure
  fixture; verifies the check does not Skip on non-zero exit.
  Message contains `FAILED`.
- `check_smart_selftest_outdated_failure_not_fail` -> single `Ok`.
  Uses the `smartctl-selftest-ata-failure-outdated.json` fixture
  whose superseding extended pass is deterministically recent
  (~50 hours). Asserts `status == Ok` AND message does NOT contain
  `FAILED`. Pins that an outdated (superseded) failure does NOT
  trigger `Fail`.
- `check_smart_selftest_passing_short_does_not_clear_extended_failure`
  -> single `Fail`. Pins the smartmontools failure-supersession
  semantic at the doctor layer.
- `check_smart_selftest_runner_spawn_failure_skips` -> single
  `Skip`. Mock runner returns
  `Err(CmdError::Failed("smartctl: not found".into()))` (the variant
  defined at `cli/src/cmd.rs:1189-1194`). Asserts the returned `Vec`
  has length 1, the row's `name == "smart_self_test"`,
  `subject == Some(<drive_name>)`, `status == Skip`, and the message
  embeds the underlying error verbatim (the substring
  `smartctl: not found` is present). Pins step 0 of the per-drive
  decision matrix and ensures missing-tool / spawn-failure scenarios
  on a real host degrade to Skip rather than panicking or being
  swallowed silently.
- `check_smart_selftest_smartctl_errors_bit_0_2_empty_stdout` ->
  single `Skip`. Runner returns `exit_status = 2` (bit 1, device
  open failed) with empty stdout.
- `check_smart_selftest_command_error_with_nonempty_stdout` -> single
  `Skip`. Runner returns `exit_status = 4` (bit 2) with parseable
  JSON stdout. Pins that `command_error` short-circuits BEFORE
  doctor reads any other field.
- `check_smart_selftest_parse_failure_skips` -> single `Skip`. Runner
  returns `exit_status = 0` with garbage stdout (`"not json"`).
  Pins the second of the ordered Skip gates.
- `check_smart_selftest_missing_power_on_time_skips` -> single
  `Skip`. Runner returns a fixture with `device.protocol = "ATA"`, a
  populated `ata_smart_self_test_log.standard.table` (recent passing
  entry), `error_count_total = 0`, `error_count_outdated = 0`, but
  NO `power_on_time` block.
- `check_smart_selftest_active_failure_without_poh_still_fails` ->
  single `Fail`. Fixture has populated
  `ata_smart_self_test_log.standard` with `error_count_total = 1`,
  `error_count_outdated = 0`, a Failed-classified entry, NO
  `power_on_time` block. Asserts `status == Fail` AND message
  contains `FAILED`. Pins gate-ordering (failure detection runs
  BEFORE missing-POH Skip).
- `check_smart_selftest_fatal_or_unknown` -> single `Fail`. Drive's
  most recent entry is status 0x3 (no `passed` field in JSON) and
  `active_errors == 1`. Pins the status-value classifier.
- `check_smart_selftest_aborted_only_warns_never` -> single `Warn`.
  Asserts the **never** form (`no completed SMART self-test
  recorded`), NOT the stale form (no `~` substring keyed off the
  age-phrase template).
- `check_smart_selftest_empty_log_warns_never` -> single `Warn`.
  Truly empty log fixture (only `count: 0`). Asserts the never form.
- `check_smart_selftest_stale_warns_with_age` -> single `Warn`. Last
  passing entry ~3000 hours old (= 125 truncated days, plural).
  Asserts the **stale** form: message contains the literal substring
  `~125 days` AND the copy-paste hint.
- `check_smart_selftest_ok_uses_singular_day_at_boundary` -> single
  `Ok`. Fixture with last passing entry at age exactly 24 hours
  (one truncated day). Asserts the message body contains the
  literal substring `passed ~1 day ago` -- specifically the
  singular `day` with no trailing `s`. Pins the
  `approx_days_phrase` boundary case and the project's
  singular/plural convention.
- `approx_days_phrase_pluralisation` (direct helper test): asserts
  `approx_days_phrase(0) == "~0 days"`,
  `approx_days_phrase(23) == "~0 days"`,
  `approx_days_phrase(24) == "~1 day"`,
  `approx_days_phrase(47) == "~1 day"`,
  `approx_days_phrase(48) == "~2 days"`,
  `approx_days_phrase(STALE_SELFTEST_THRESHOLD_HOURS) == "~90 days"`.
  Pins each side of the singular boundary plus the truncation
  invariant.
- `check_smart_selftest_nvme_skips_with_protocol_reason` -> single
  `Skip`. Drive's `device.protocol = "NVMe"`; reason string contains
  "NVMe".
- `check_smart_selftest_scsi_or_missing_protocol_skips` -> single
  `Skip`. Drive's `device.protocol = "SCSI"` (or absent). Reason
  names the protocol the parser observed.
- `check_smart_selftest_active_errors_fallback_message` -> single
  `Fail`. Mock parser output has `active_errors = 1` and
  `last_failure: None`. Message MUST be the fallback form
  (`reports <N> active failure(s) but no failure entry was parsed`),
  NOT the detail form.

Multi-drive and structural tests (the new emit-one-per-drive shape
needs explicit coverage):

- `check_smart_selftest_emits_one_result_per_drive`: three-drive
  pool, all with the recent-pass fixture. Asserts the returned `Vec`
  has length 3, all entries have `name == "smart_self_test"`, all
  entries have `subject == Some(<drive_name>)`, and the set of
  subjects equals the set of pool member names exactly. Pins the
  fundamental shape change from aggregate to per-drive.
- `check_smart_selftest_preserves_membership_order`: three-drive
  pool. Asserts the returned `Vec`'s `subject`s appear in the same
  order as `iter_by_name()` so doctor output is stable across runs.
- `check_smart_selftest_mixed_statuses_one_per_drive`: three-drive
  pool: drive1 recent-pass (Ok), drive2 stale (Warn), drive3 active
  failure (Fail). Asserts three `CheckResult`s with distinct
  `subject`s, each carrying the per-drive status and message body
  from the matrix. The recent-pass drive does NOT borrow any
  problem-detail text; each row stands alone.
- `check_smart_selftest_membership_load_error_emits_unscoped_skip`:
  pool.json missing or unreadable. Asserts the returned `Vec` has
  length 1, the single result has `name == "smart_self_test"`,
  `subject == None`, `status == Skip`, and message mentions the
  underlying enumeration failure (e.g., `"pool membership not
  enumerable"`). Pins the only `subject: None` shape this check
  emits.
- `check_smart_selftest_no_members_emits_unscoped_skip`: pool.json
  parses but enumerates zero members. Same shape as above: one
  unscoped `Skip` row naming the empty-membership condition.
- `check_smart_selftest_message_contains_by_id_path`: the copy-paste
  hint in `Warn` messages contains the literal
  `/dev/disk/by-id/<name>` path verbatim. Independent of the
  `subject` field -- the hint must be paste-ready by itself.

Formatter test (`format_doctor_human_with`):

- `format_subject_rendered_after_label`: synthesize a `CheckResult`
  with `name = "smart_self_test"`, `subject = Some("disk1")`,
  `status = Ok`, `message = "passed ~2 days ago"`. Render through
  `format_doctor_human_with` and assert the line contains
  `smart selftest disk1` (label + subject joined by single space)
  followed by the message text. Independent companion test
  `format_subject_none_renders_existing_shape`: a `CheckResult` with
  `subject = None` (mimicking any existing check) renders exactly
  the bare-label format and is byte-identical to the current
  formatter's output for that input.

JSON serialization tests (sit next to the existing
`json_serialization_lowercase` test at `cli/src/doctor.rs:1560-1575`):

- `json_serialization_subject_none_omits_field`: serialize a
  `CheckResult` with `subject = None` and assert the resulting JSON
  string does NOT contain the substring `"subject"`. Pins the
  `skip_serializing_if = "Option::is_none"` contract directly so a
  future refactor cannot accidentally start emitting
  `"subject": null` and silently change the wire shape for every
  existing check.
- `json_serialization_subject_some_emits_field`: serialize a
  `CheckResult` with `subject = Some("disk1")` and assert the
  resulting JSON contains the literal substring `"subject":"disk1"`.
  Pins the positive case: per-drive `smart_self_test` rows are
  observable on the wire.
- `json_roundtrip_preserves_subject`: serialize then deserialize a
  `CheckResult` with each of `subject = None` and `subject = Some("disk1")`;
  assert the round-tripped struct equals the original. Pins the
  `#[serde(default)]` attribute (without which, a missing JSON
  `subject` would fail to deserialize for the existing checks).

### VM test

No new VM test file. The existing `tests/cli/braid-doctor.py` runs
`braid doctor` against a NixOS VM and asserts JSON shape. Two
extensions:

1. **Lookup-by-name no longer works for the new check.** The existing
   pattern at `tests/cli/braid-doctor.py:34` builds a `dict` keyed by
   `name`:
   `checks = {c["name"]: c for c in report["checks"]}`. With
   multiple `smart_self_test` rows, that dict comprehension would
   silently drop all but one row (last-wins). Add a separate
   list-based filter for this check:

   ```python
   selftest_rows = [c for c in report["checks"] if c["name"] == "smart_self_test"]
   ```

2. **Assert structural invariants, not values.** Virtio disks produce
   unpredictable smartctl behaviour, so the test asserts shape only.
   The serialization contract (per the `cli/src/doctor.rs` section)
   omits the `subject` JSON field when it is `None`, so the
   assertions must distinguish "field absent" from "field present and
   non-empty":
   - `len(selftest_rows) >= 1` (the check fires).
   - If `len(selftest_rows) == 1` AND `"subject" not in selftest_rows[0]`:
     this is the unscoped membership-failure case. Assert
     `selftest_rows[0]["status"] == "skip"` and the message names the
     enumeration failure.
   - Otherwise (the per-drive case): every row has `"subject" in row`
     AND `row["subject"]` is a non-empty string equal to one of the
     declared pool members in the test VM. The set of subjects has no
     duplicates. No row in this branch carries an absent `subject`
     (per-drive rows always set it).
   - In both branches, every row's `name` equals `"smart_self_test"`.

   Do not assert on `status` values for the per-drive branch --
   virtio outcomes are indeterminate.

## Style invariants

- All user-facing strings use `--` (double hyphen), not `—` (em-dash). See
  `docs/luks-unlock.md` and `cli/src/luks.rs:706,742` for the established
  copy-paste hint shape.
- Check `name` uses snake_case to match existing doctor checks
  (`config_file`, `declared_disks`, `pool_missing_devices`, ...):
  `smart_self_test`. The name is **stable across all per-drive rows**
  -- the disk identity goes into the `subject` field, never into
  `name`. Avoid `smart_self_test_<diskname>` shapes: dynamic JSON
  keys break enumeration in machine consumers.
- The hint string contains the literal `/dev/disk/by-id/<name>` path -- no
  placeholders -- so the user can paste it unmodified.
- Per-drive messages do NOT prefix the disk identity into the message
  body. The disk is rendered via `subject`, not via repeated
  `<name>: ...` text. Messages are bare predicates (`passed ~2 days
  ago`, `no completed SMART self-test recorded -- ...`).
- New `pub` items get a `///` doc comment per the project's doc-comment
  policy (intent / invariant, not signature restatement).

## Verification

1. `just test-rust` -- new parser and doctor unit tests pass.
2. `just test-vm braid-doctor` -- existing VM doctor test still passes
   and the new check appears in the JSON output for the test pool.
3. Manual smoke on a real host (not automated):
   - `sudo braid doctor` on a host that has never run a self-test ->
     check reports `Warn` with the `smartctl -t short ...` hint.
   - `sudo smartctl -t short /dev/disk/by-id/<one-drive>`, wait for
     completion (~1-5 min), re-run `sudo braid doctor` -> that drive's
     line flips to `Ok`.
4. Fixture policy: the selftest fixtures are hand-authored and not
   regenerated by `just capture-all-fixtures`. Smartmontools is
   parser-critical (see `AGENTS.md`), so a smartmontools or nixpkgs
   bump that affects `ata_smart_self_test_log.standard` JSON shape is
   a required-refresh event for these fixtures -- update them by hand
   and re-run `just test-rust`.
