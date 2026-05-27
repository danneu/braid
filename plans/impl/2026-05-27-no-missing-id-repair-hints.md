# Pivot: unify the missing-device repair hint on the no-`--missing-id` form

## Context

A review finding flagged that the "disk not found" hint in `remove.rs` says
bare `` `braid replace` `` while `check_no_missing_devices` says
`` `braid replace --missing-id <devid>` ``, and proposed adding `--missing-id`
to `remove.rs` so they match.

Investigation showed the finding had the direction backwards. `--missing-id`
is an **optional cross-check, never required**: `resolve_replace_source`
(`cli/src/replace.rs:1735-1762`) auto-resolves the dead disk's devid from
`--old`'s `pool.json` entry, and `--missing-id` (when supplied) must *equal*
that persisted devid or it is refused. This is stated authoritatively in:

- `docs/design/decisions/012-intent-cli.md:66` (Replace safety constraints):
  "`--old`'s name already identifies the member ... `--missing-id` is an
  optional cross-check ... and is never required."
- `docs/commands/replace.md:14-17,72`: "no mode to choose and `--missing-id`
  is never required" / "Never required."
- `cli/src/status.rs:1399-1401`: the most operator-facing surface already
  omits `--missing-id`, but still uses the shortened, non-runnable placeholder
  `braid replace --old {name} --new <new-name>` instead of the CLI's
  `--new <name>=<path>` shape.

The problem is **drift, not a competing design**: roughly eight runtime hints
plus drifted doc prose still carry the redundant
`braid replace --missing-id <devid>` form, contradicting the constraint
decision 012 itself states one line up (012:67/93 contradict 012:66). Leaving
the doc prose unfixed means a future agent reading 012:93's
"(preferred)" will re-add `--missing-id` to the code and undo this work.

**Outcome:** every surface names the same runnable repair command --
`braid replace --old <name> --new <new-name>=/dev/disk/by-id/<...>` --
produced by one shared helper, and the decision docs stop contradicting
themselves. This dissolves a recurring class of review finding instead of
patching one site.

## Approach

1. **Add one shared helper** -- the single source of truth for the repair
   command, mirroring the existing `luks_uuid_mismatch_guidance()` precedent
   (`cli/src/luks.rs:709-713`, a `&'static str` phrase embedded by callers).

   Home: `cli/src/preflight.rs`, next to `check_no_missing_devices`
   (the existing canonical missing-device gate; already imported widely).

   ```rust
   /// Canonical missing-device repair command. Single source of truth so
   /// every boundary (status, doctor, preflight gates, add/replace/remove
   /// warnings) names the same runnable form, enforcing decision 012:66's
   /// "`--missing-id` is never required". Pass the missing member's name
   /// when the site knows it; pass `None` for generic gates that do not.
   pub(crate) fn replace_repair_command(old: Option<&DiskName>) -> String {
       match old {
           Some(name) => {
               format!("braid replace --old {name} --new <new-name>=/dev/disk/by-id/<...>")
           }
           None => {
               "braid replace --old <name> --new <new-name>=/dev/disk/by-id/<...>".to_string()
           }
       }
   }
   ```

   Callers embed the returned command into their own surrounding sentence
   (count, pluralization, `recover`-first preamble, "or forget with
   `braid remove-missing`", "then retry", "Consider ... first"). The helper
   owns only the command string, exactly like `luks_uuid_mismatch_guidance`.

2. **Route every in-scope hint through the helper, dropping `--missing-id`.**
   Sites that know the missing disk's name pass it; generic gates pass `None`
   and add "run `braid status` to see the missing disk's name".

3. **Fix the drifted doc prose** to match decision 012:66.

4. **Regenerate and classify the `--missing-id` test inventory** from a
   tracked-file search before editing assertions:
   `git grep -n -e '--missing-id' -- cli/src tests docs README.md`. Update
   only repair-hint wording pins and their preambles; leave valid
   `remove-missing --missing-id` requirements and valid `replace --missing-id`
   cross-check/rejection behavior unchanged.

### Scope distinction (critical)

This sweep changes **remediation recommendations** ("use X to repair"). It does
**not** touch docs that *describe what `--missing-id` does* -- the flag still
exists. Leave untouched: `docs/commands/replace.md` (documents `--missing-id`
as an optional advanced flag, already correct), and the behavioral references
at `replace.md:28,110-112`, `docs/guides/recovery-scenarios.md:240-241`,
`docs/commands/status.md:258`.

## In-scope hint sites

Each: drop `--missing-id`, emit `replace_repair_command(...)`, keep
site-specific surrounding context.

| Site | Current repair clause | Name known? |
|---|---|---|
| `cli/src/preflight.rs:293-307` (`check_no_missing_devices`) | `braid replace --missing-id <devid>` | No -> `None` |
| `cli/src/remove.rs:547-557` (disk-not-found + missing detected) | bare `braid replace` | No -> `None` |
| `cli/src/replace.rs:1668-1676` (live replace blocked by missing) | `braid replace --missing-id <devid>` | No -> `None` |
| `cli/src/add.rs:876-883` (`format_add_missing_devices_warning`) | `braid replace --missing-id <devid>` | No -> `None` |
| `cli/src/pool.rs:328-333` (`RemoveContext::Missing`) | `braid replace --missing-id <devid>` (keep `recover`-first preamble) | No -> `None` |
| `cli/src/doctor.rs:752-758` (`pool_missing_devices`) | `braid replace --old <disk> --new <disk> --missing-id <devid>` (also fixes the `--new <disk>` placeholder bug; keep "(devid N: ...)") | No -> `None` |
| `cli/src/remove_missing.rs:414-425` (2-disk RAID1 rejection) | `... --new <new-name>=... --missing-id {devid}` (keep "or `braid add` then re-run", "see device names and IDs") | No -> `None` |
| `cli/src/status.rs:1399-1401` (per-missing-member Action) | no `--missing-id`, but shortened invalid `--new <new-name>` placeholder | Yes -> `Some(name)` (update expected output to full `--new <new-name>=/dev/disk/by-id/<...>`) |

### No change needed

- `cli/src/mount.rs:16` (`DEGRADED_MOUNT_WARNING`): terse status line, already
  bare `braid replace` (no `--missing-id`) -- already on the correct side.
  Leave as-is.

### Explicitly excluded

- `cli/src/luks.rs:709-713` (`luks_uuid_mismatch_guidance`): a present-disk,
  wrong-UUID scenario -> a **live** replace (`--old`/`--new`), which correctly
  takes no `--missing-id`. Different problem domain. Do not touch.

## Doc prose to fix (match decision 012:66)

- `docs/design/decisions/012-intent-cli.md:67` -- drop `--missing-id` from
  "repair ... first with `braid replace --missing-id <devid>`".
- `docs/design/decisions/012-intent-cli.md:93` -- "`braid replace
  --missing-id <devid>` (preferred)" -> "`braid replace` (preferred)".
- `docs/design/decisions/001-btrfs-raid1.md:53` -- "via `braid replace
  --missing-id <devid>`" -> "via `braid replace` (the missing devid
  auto-resolves from `--old`)".
- `docs/commands/remove-missing.md:80` -- drop trailing `--missing-id <devid>`
  from the repair command (keep `--old <missing-name> --new <new-name>=...`).
- `docs/guides/recovery-scenarios.md:265-270` -- remove the "may need the
  btrfs devid" paragraph and `--missing-id 3` snippet from dead-disk Option A;
  keep the no-`--missing-id` command with
  `--new toshiba4=/dev/disk/by-id/ata-NEW_DRIVE_SERIAL` as the repair path.

## Test changes

Regenerate this inventory immediately before implementation with
`git grep -n -e '--missing-id' -- cli/src tests docs README.md`, then split
hits into the two buckets below. Do not update every `--missing-id` assertion
mechanically.

**Repair-hint wording pins to update:**

- `cli/src/preflight.rs:1282` -- old `check_no_missing_devices` repair
  guidance.
- `cli/src/doctor.rs:4826` -- degraded-pool recommendation.
- `cli/src/pool.rs:1364,1467,1503` -- missing-device min-devices hints;
  update the positive assertions and strengthen the live-context negative
  assertion so it does not keep checking only the old `braid replace
  --missing-id` spelling.
- `cli/src/remove_missing.rs:950,1013` and nearby preambles/comments around
  `cli/src/remove_missing.rs:406,419` -- 2-disk RAID1 repair guidance should
  name `braid replace --old ... --new ...`, not the replace `--missing-id`
  flag.
- `cli/src/replace.rs:2296` -- mixed live+missing replacement rejection must
  recommend the shared repair command. Leave
  `cli/src/replace.rs:2266`'s live-old `--missing-id cannot be used` assertion
  unchanged.
- `cli/src/add.rs:9013` -- exact rendered warning line from
  `format_add_missing_devices_warning`.
- `cli/src/status.rs:5258,5319` -- status tests currently pin the invalid
  shortened `--new <new-name>` placeholder.
- `tests/cli/braid-remove-disk.py:239` -- strengthen from bare
  `braid replace` to the full new repair-command contract.
- `tests/cli/replace-live-disk.py:225` -- mixed-state repair guidance. Leave
  `tests/cli/replace-live-disk.py:211` unchanged because it asserts live
  replace rejects the incompatible `--missing-id` flag.
- `tests/cli/remove-missing-2disk-rejected.py:8,29,103,120` -- preamble and
  output assertions for the repair hint. The command under test still uses
  `remove-missing --missing-id`.
- `tests/cli/braid-add-warnings.py:79,100,173` -- exact warning text and
  preamble.

**Valid `--missing-id` behavior to leave unchanged:**

- `remove-missing` still requires `--missing-id`; leave CLI help/completion,
  docs, module tests, and assertions such as
  `tests/cli/braid-remove-disk.py:258` unchanged.
- `replace --missing-id` remains a valid optional cross-check for dead-disk
  replacement and remains rejected for live replacement; leave the explicit
  dead-path tests (`tests/cli/replace-dead-disk.py`,
  `tests/cli/replace-rejects-smaller-target.py`) and live rejection assertion
  `tests/cli/replace-live-disk.py:211` unchanged.
- Keep behavioral docs that explain the flag, including
  `docs/commands/replace.md` and status docs that explain where device IDs are
  used.

**Test bar (behavioral, structure-insensitive):** for each touched hint, assert
the user-visible repair hint (a) names `braid replace --old` ...
`--new <new-name>=/dev/disk/by-id/<...>`, and (b) does **not** instruct
`--missing-id`. Clause (b) is the enforced behavioral contract of this pivot
(operators are never told to pass a flag that is never required) and is about
output wording, not internal structure -- so it is a legitimate assertion, not
a brittle implementation pin. Add one focused unit test on
`replace_repair_command` itself asserting the full placeholder shape for both
the named and `None` branches.

## Verification

1. `just test-rust` -- unit tests, including the new helper test and the
   updated `preflight`/`doctor`/`pool`/`remove_missing`/`replace`/`add`/`status`
   hint assertions.
2. Targeted VM tests for the touched CLI hint paths (not the full suite):
   `just test-vm braid-remove-disk replace-live-disk remove-missing-2disk-rejected braid-add-warnings`.
3. Doc cross-links: `mdbook build docs` (per AGENTS.md, broken cross-links fail
   CI; the doc edits are prose-only but build
   confirms nothing breaks).
4. Sanity grep -- rerun
   `git grep -n -e '--missing-id' -- cli/src tests docs README.md` and confirm
   every remaining hit is either valid flag behavior/documentation or a
   negative assertion that the new repair hints do not instruct `--missing-id`;
   no repair-path recommendation should still carry `--missing-id`.
