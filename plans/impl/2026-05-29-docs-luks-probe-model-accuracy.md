# Docs/comments accuracy sweep: the LUKS header-probe model

## Context

A code-review finding flagged `cli/src/probe.rs:198-208`: it claimed a LUKS2
header damaged enough to fail braid's `luksDump` version/label parse -- but not
`cryptsetup luksUUID` -- becomes a hard `ProbeError` that `--allow-degraded`
cannot bypass, stranding an otherwise-mountable degraded pool.

Investigating that finding against the pinned cryptsetup source disproved its
premise **and** uncovered a wider, shared-root inaccuracy in braid's comments:
many comments/tests describe `cryptsetup isLuks` / `luksUUID` / `luksDump` as if
they probe progressively deeper layers of the header and can therefore disagree
about a damaged disk. They cannot. This is a **docs/comments-accuracy** change:
behavior is unchanged. Goal: every comment describing the probe model matches how
cryptsetup actually behaves, so this class of finding stops recurring. The one
genuine *behavioral* consequence (an ineffective `Damaged`/`Unreadable`
discriminator) is documented honestly and handed off as a separate design pass --
see **Follow-up**.

One narrow, non-comment exception is included: two focused argv unit tests that
pin the premise the whole model rests on (see edit set, item F4).

## Verified ground truth (pinned cryptsetup, in `reference/cryptsetup/`)

| # | Fact | Evidence |
|---|------|----------|
| C1 | braid runs `isLuks`, `luksUUID`, `luksDump` with **no** `--type`. | `cli/src/cmd.rs:471,595,893` (pinned by tests -- see F4) |
| C2 | All three gate on the **same** `crypt_load(cd, luksType(NULL), NULL)`. `isLuks` returns its result directly; `luksUUID` additionally needs `crypt_get_uuid()` non-NULL; `luksDump` additionally runs `crypt_dump()`. | `src/cryptsetup.c:2479` (isLuks), `:2497` (luksUUID), `:2639` (luksDump) |
| C3 | The LUKS2 JSON validation -- and, when locking is enabled, auto-recovery + blkid signature probing -- happens **inside `crypt_load`**, not in `luksDump`. A header with no usable copy, **or a recovery/blkid abort even when one copy is valid** ("ambiguous signatures, cannot auto-recover"), fails `crypt_load`. (So whether load succeeds is *not* a simple "at least one copy validates".) | `lib/setup.c` `_crypt_load_luks2`; `lib/luks2/luks2_disk_metadata.c` `LUKS2_disk_hdr_read` (dual-copy read at :662-693; signature-abort `goto err` at :725-729 / :746-750) |
| C3b | That auto-recovery is a **write** -- it rewrites the damaged copy from the good one -- and is attempted even for braid's plain read commands (`LUKS2_hdr_read` passes `do_recovery=1`, and re-takes a **write lock** to retry recovery). So `crypt_load`, hence `isLuks`/`luksUUID`/`luksDump`, is **not strictly read-only**: probing a one-good-copy header may mutate it. | `lib/luks2/luks2_disk_metadata.c:722-763` (`hdr_write_disk`); `lib/luks2/luks2_json_metadata.c` `LUKS2_hdr_read:1176-1189`; `lib/setup.c` `_crypt_load_luks2:763` (`repair=0` on plain load) |
| C4 | Once `crypt_load` succeeds, braid's text `luksDump` is **effectively infallible**: `crypt_dump -> LUKS2_hdr_dump` returns 0 unless `hdr->jobj` is NULL, which a successful load always populates; the sub-dump helpers return void. | `lib/setup.c:6185`; `lib/luks2/luks2_json_metadata.c:2190-2213` |
| C5 | **Therefore, on a stable device the three commands always agree** -- they share the same `crypt_load` outcome and post-load dump can't fail. They diverge only under a transient fault (I/O error, OOM) **on the second, separate process invocation**, or a concurrent header rewrite between the two invocations. | C2-C4 |

`run()` semantics that make C5 concrete (`cli/src/cmd.rs:1259-1285`): a process
that executes returns `Ok(RawCommandOutput{exit_status})` even on non-zero exit;
only a spawn/signal failure returns `Err(CmdError)`. So in `probe_config_disk`,
**a luksDump spawn failure -> `ProbeError::Cmd`**, while **any non-zero luksDump
exit (including a transient I/O fault or concurrent rewrite on the second call)
is parsed as `ParseError::CommandFailed` -> `ProbeError::Parse`**.

Two consequences this sweep documents:

- **The original finding's window is unreachable.** Genuine LUKS2 metadata damage
  fails `luksUUID` (= `crypt_load`) first -> `PresentNotLuks`, which
  `plan_open_pool_inner` already routes into the degraded-refusal `missing`
  vector (so `--allow-degraded` works). See the reachable-cause list in F1 below
  for what actually reaches the gateway hard-error branch.
- **`LuksHeaderState::Damaged` (isLuks ok + luksDump fail) is also unreachable on
  a stable disk** for the same reason. It maps to distinct `status`/`doctor`/TUI
  output and distinct guidance ("try `cryptsetup repair`" vs. "restore from
  off-system backup", `cli/src/doctor.rs:412,418`), so that "repair" guidance is
  effectively dead and real corruption is routed to the "Unreadable" path.

## Scope

- **In:** Rust code comments, `///` docstrings, and test-scenario preambles that
  mis-describe the probe model, **plus** two focused argv unit tests (F4). No
  command logic, error mapping, user-facing strings, or existing test assertions
  change.
- **Out (deliberately):**
  - Any behavior change, including collapsing the redundant `isLuks`+`luksDump`
    probe or re-routing guidance -- that is the **Follow-up** design pass.
  - End-user reference docs that *list* the `DAMAGED` disk state
    (`docs/commands/status.md:164-165`, `docs/commands/doctor.md:71,100`). They
    document the enum surface, which still exists; revising them belongs with the
    behavioral pass. Called out so the boundary is explicit.

## The canonical correction (anchor once, reference everywhere)

To avoid re-deriving wording at ~20 sites, fix the model in **one** authoritative
place and have the rest point to it or carry a one-line accurate form.

**Anchor:** the `probe_luks_header` docstring (`cli/src/luks.rs:672-676`). Add a
NOTE stating: `isLuks` and `luksDump` both gate on the same cryptsetup
`crypt_load` (braid passes no `--type`), so on a stable device they always agree
and the second probe is redundant except as a transient-fault detector;
`crypt_load` -- not `luksDump` -- performs JSON validation/recovery, and a LUKS2
dump is infallible once load succeeds. Consequence: the `Ok`/`Unreadable`/
`Damaged` split cannot distinguish "recognized-but-metadata-damaged" from
"unrecognized" the way the variant names imply -- genuine corruption fails
`crypt_load` (hence `isLuks`) and surfaces as `Unreadable`; `Damaged` arises only
from a transient fault between the two invocations. Note that reworking this is an
open design question left to a separate change, and that current behavior is
unchanged.

**Also fix two now-false claims in this file (per C3/C3b):**
- Drop the "Read-only LUKS header probe" lead on `probe_luks_header`
  (`cli/src/luks.rs:672`). braid does not intentionally format or open the
  device, but cryptsetup's `crypt_load` may **auto-recover (write)** a damaged
  LUKS2 header copy, so the probe is not strictly read-only. Reword to that
  effect; keep the still-true "reads the raw block device, not the mapper" point.
- The `LuksHeaderState::Ok` variant doc (`cli/src/luks.rs:658`) must not say "the
  header is intact." `Ok` means `crypt_load`+dump succeeded -- which may include a
  header that was auto-recovered, or whose primary copy is bad but secondary is
  good -- not that every on-disk copy was pristine.

**One-line accurate form** for sites that need their own sentence:
"`isLuks` and `luksDump` share `crypt_load`, so `Damaged` (isLuks ok + luksDump
fail) is not a distinguishable on-disk state -- corruption fails both (->
`Unreadable`); only a transient fault lands here (see `probe_luks_header`)."

**Correction rule per hit** (preserve behavior; fix only the description):
- Replace any claim that `isLuks` is a magic-only/lighter check, that "magic is
  intact but metadata is damaged", or that `Damaged` is a stable, reachable,
  "repairable in place" corruption state -> the one-line form / anchor ref.
- For guidance/label mapping docs (which state -> which message), keep the mapping
  (behavior is unchanged) but drop the false causation; map by state name, not by
  "magic intact vs gone".
- For test preambles that assert a Scenario producing `isLuks ok + luksDump fail`
  from real corruption, relabel the Scenario as **synthetic** (a mock standing in
  for a transient fault); leave the assertion and the mapping-under-test intact.
- Precedent to match: `cli/src/status.rs:1095-1105` already frames this correctly
  ("avoid overclaiming `Damaged`") -- align new wording to it.

## Edit set

### F1 -- `cli/src/probe.rs` gateway hard-error comment (~188-198), keystone

Rewrite the block. Keep the LUKS2-only-invariant rationale and the "gateway must
not report unconfirmed disks as healthy `PresentLuks`" point. Drop the misleading
"(typically damaged LUKS2 metadata)". Add that this branch is **not** how genuine
metadata damage surfaces (C2-C5): real damage fails the `luksUUID` above ->
`PresentNotLuks` -> degraded-refusable. State the **complete** reachable-cause
list for hitting this branch after `luksUUID` succeeded:
1. `luksDump` fails to spawn -> `ProbeError::Cmd` (environmental).
2. `luksDump` runs but exits non-zero -- a **transient I/O/OOM fault on this
   second invocation, or a concurrent header rewrite** between the two calls --
   parsed as `CommandFailed` -> `ProbeError::Parse`.
3. `luksDump` exits 0 but the `Version:` line is unparseable -- cryptsetup output
   drift, a parser-compat signal that must stay loud -> `ProbeError::Parse`.
4. A LUKS1 device (`Version: 1`) -> `UnsupportedLuksVersion`.
All are correctly fail-closed.

### F2 -- folded into F1 above

The reachable-cause list now includes transient second-invocation failures and
concurrent rewrites, and distinguishes spawn (`Cmd`) from non-zero exit (`Parse`),
consistent with C5 and `run()` semantics.

### F3 -- inventory-driven sweep (replaces the earlier hand-enumeration)

Derive the inventory from tracked files; do not rely on this list being complete.

**Sweep commands** (run, fix every hit per the correction rule, then re-run as
verification). **Use both pathspecs** -- `'cli/src/**/*.rs'` alone silently drops
the top-level `cli/src/*.rs` files (verified: 53 hits, zero of luks/mount/probe/
doctor/status/recover/types.rs), which are exactly the central files this sweep
must cover; the two-pattern form returns all 84:
```
git ls-files 'cli/src/*.rs' 'cli/src/**/*.rs' ':!cli/src/parse/**' \
  | xargs rg -n 'magic|metadata.{0,15}(damag|corrupt|read)|(damag|corrupt).{0,15}(metadata|header|in place)|repairable|cryptsetup repair|isLuks (ok|succe|exits|fails)|LuksHeaderDamaged|LuksHeaderUnreadable'
```
Also re-grep the literal stale phrases and confirm zero remain outside the anchor:
`magic is gone`, `magic (is )?intact`, `magic (missing|absent)`,
`metadata is damaged`, `repairable in place`, `potentially repairable`.

**Confirmed sites (representative, grouped by file)** -- correction-needed unless
marked:
- `cli/src/luks.rs` -- `probe_luks_header` docstring incl. the "Read-only" lead
  (anchor, ~672); `enum LuksHeaderState` terminology contract + `Ok` ("intact"),
  `Unreadable`, `Damaged` variant docs (~653-670); optional one-line note on
  `luks_header_damaged_guidance` (~733).
- `cli/src/mount.rs` -- `MissingReason` variant docs (47-55); `PresentNotLuks`
  refinement comment (253-266); guidance-dispatch doc (445-448); `Damaged`
  format test preamble (1526-1534) and the footer tests' scenarios (1552-1617,
  relabel scenarios only).
- `cli/src/probe.rs` -- F1 block (188-198). No change to
  `probe_config_disk_luksdump_failure_propagates_as_cmd_error` /
  `..._garbled_propagates_as_parse_error` (1300/1338): their scenarios (spawn
  failure, output drift) are already accurate.
- `cli/src/doctor.rs` -- `DiskState::LuksHeaderUnreadable`/`Damaged` docs
  (307-312); doctor test preambles (3166, 3212-3217, 3260).
- `cli/src/status.rs` -- verbose `LuksHeaderUnreadable`/`Damaged` test preambles
  (2788-2862, 4884-4924); leave 1095-1105 as-is (already correct; it is the
  precedent).
- `cli/src/recover.rs` -- test preamble at ~18338 (luksUUID fails / isLuks ok /
  luksDump fail scenario).
- `cli/src/tui/probe.rs` -- refinement comment (417-428); `Unreadable` test
  (2326-2335) and `Damaged` test (2395-2404).
- `cli/src/tui/model.rs` -- `LuksHeaderDamaged` render-variant doc (278-279).
- `cli/src/tui/view/mod.rs` -- "corrupted header" comment (~2352); verify wording.
- `cli/src/types.rs` -- `ConfigDiskState::PresentNotLuks` doc (506-507): broaden
  to "`luksUUID` failed -- not LUKS-formatted, or a LUKS header `crypt_load`
  cannot read/validate -- refined into Unreadable/Damaged for diagnostics while
  add/replace keep the coarse state."

### F4 -- pin C1 with argv tests (the one non-comment change)

Add focused `CmdRequest::to_argv` tests mirroring the existing
`cryptsetup_luks_dump_text_generates_correct_argv` (`cli/src/cmd.rs:2708-2716`),
for `CryptsetupIsLuks` and `CryptsetupLuksUuid`, asserting `["isLuks", dev]` and
`["luksUUID", dev]` respectively (no `--type`). These make the C1/C2 "identical
crypt_load gate" premise a structural invariant: adding `--type` to either later
fails a test, not just silently invalidating the comments.

## Follow-up (out of scope -- separate decision, now better motivated)

The expanded inventory is itself the argument: the stale model is duplicated
across ~20 sites in 9 files. `isLuks` and `luksDump` sharing `crypt_load` makes
the `Damaged`/`Unreadable` discriminator ineffective -- a genuinely
corrupt-but-present LUKS2 header is reported `Unreadable` ("restore from backup")
rather than `Damaged` ("try `cryptsetup repair`"). Worth a separate
`verify-issue`/decision on whether to (a) collapse the redundant second probe and
drop `Damaged`, or (b) distinguish real damage another way. Option (a) would
*delete* most of the surface this sweep is about to re-document, so if the user
expects to pursue the behavioral fix soon, doing it first would avoid churn. This
plan only makes today's docs truthful; it does not pick (a) or (b).

## Verification

- `just test-rust` -- comment/preamble changes plus the two new argv tests; the
  suite must pass. Touched test preambles change only their `//`/`///` text, not
  assertions.
- Re-run the **exact** F3 sweep command (the two-pathspec form -- not the
  `**`-only glob, which skips the central top-level files) and the literal-phrase
  greps; confirm zero stale-model hits remain outside the anchor docstring.
- `cargo build` (or rely on `just test-rust`) to confirm no docstring/attribute
  was malformed. Do **not** run `cargo fmt`.
- No `docs/` mdBook content changes, so `mdbook build docs` / linkcheck is not
  required.
- Spot-read the final `probe_luks_header` anchor and `cli/src/probe.rs` block
  against the C1-C5 table to confirm no new claim overstates reachability or
  re-specifies the `crypt_load` success condition (the failure mode -- including
  F1 -- this sweep exists to prevent).

## Implementation notes

- Swept `cli/src/discover.rs` too, though it was not in F3's enumerated
  inventory (per F3's "do not rely on this list being complete"). Reframed the
  `discover_warns_when_uuid_value_contains_split_delimiter` scenario from "a
  corrupted LUKS2 header where the UUID: line reads ..." to "a luksDump whose
  UUID: line reads ..." -- corruption fails `crypt_load`, so it cannot yield a
  parseable-but-garbage dump; the test mocks the dump output directly anyway.
  Left `discover_propagates_runner_error_at_luksdump` (discover.rs:~819)
  unchanged: its scenario ("transient I/O error on the second invocation") is
  already consistent with the corrected model.
- Several plan-listed sites turned out already accurate and were left
  unchanged after evaluation, not missed: the `tui/probe.rs` PresentNotLuks
  refinement comment (~417-426), the `doctor` unreadable preamble Intent
  ("isLuks fails"), the `tui/model.rs` `Unreadable` variant doc, the
  `tui/view/mod.rs` "corrupted header" render-test doc, the `status` verbose
  unreadable-disk preamble, the `mount` footer-presence test scenarios (keyed
  by state name), and the `status` inconsistent-fallback preamble (the existing
  precedent). The correction rule only rewrites false model claims.
- Did not rename the test fn `unlock_damaged_luks2_metadata_fails_at_gateway`
  (mount.rs) -- a fn rename is outside this sweep's comment-only + F4-argv
  scope. Only its preamble/inline comment were corrected. See Follow Up.
- New comment text uses ASCII `--` (per AGENTS.md and newer code in these
  files) rather than the older `—`. Kept the `→` arrows in the `mount.rs`
  guidance-dispatch doc as-is to avoid mixing arrow styles within one block.

## Follow Up

- Behavioral pass (the plan's motivating follow-up): decide whether to (a)
  collapse the redundant `isLuks`+`luksDump` probe and drop `LuksHeaderState::Damaged`,
  or (b) distinguish real damage another way. As documented now, `Damaged` is
  unreachable from genuine corruption, so `luks::luks_header_damaged_guidance`
  and the `cryptsetup repair` path it feeds are effectively dead code.
- End-user reference docs still list the `DAMAGED` disk state
  (`docs/commands/status.md:164-165`, `docs/commands/doctor.md:71,100`).
  Deliberately out of scope here (they document the enum surface, which still
  exists); revise alongside the behavioral pass.
- Rename `unlock_damaged_luks2_metadata_fails_at_gateway` (`cli/src/mount.rs`)
  to match its corrected scenario -- it pins luksDump-failing-after-luksUUID-
  succeeds at the gateway, not genuine metadata damage. Folds naturally into
  the behavioral pass if `Damaged` is dropped.
