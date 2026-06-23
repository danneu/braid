# Plan: severity-first alert banner rename

## Context

`braid status` renders a latched-alert banner in two severity tiers. Today the
two tiers use unrelated words and unrelated sentences:

- Critical: `ALERT -- disk health issue detected. Run 'braid ack' to acknowledge and silence.`
- Warning:  `NOTICE -- capacity risk detected. Run 'braid ack' to acknowledge.`

**The screen vocabulary disagrees with the severity enum.** The enum is
`AlertSeverity::{Warning, Critical}` (`cli/src/alert.rs`), but the Warning tier
renders the word `NOTICE`. The only reason it isn't `WARNING` is that advisories
already render with a `warning:` prefix (`format_status_human`), and an
ENOSPC-risk pool emits *both* the latched banner and a live `warning: ENOSPC
risk: ...` advisory at once -- so `WARNING` would have collided with `warning:`.
A severity-first shape (`<SEVERITY> alert -- ...`) dissolves that collision, lets
the Warning tier say `WARNING`, and makes the screen vocabulary match the enum.

This is primarily a **rename**: change the banner words, then sync the docs and
ADR that quote them. While the `### Alert banner` section is open for the rename,
also close two small residual gaps in it -- the cause-line example inventory is
missing two of the six shapes (`missing device`, `alert computation error`), and
the section doesn't spell out the Warning-tier `ack` distinction (acknowledge vs
acknowledge-and-silence) or the ENOSPC post-ack snooze.

**Outcome:** a consistent, severity-first banner whose words match the enum, with
`status.md` and ADR 014 re-synced to the new strings and the banner section's
cause-line inventory and ack semantics completed.

## Decision: final banner strings (Option B, severity-first)

```
CRITICAL alert -- pool health issue detected. Run 'braid ack' to acknowledge and silence.
WARNING alert -- capacity risk detected. Run 'braid ack' to acknowledge.
```

- Severity word leads, so the loud/quiet gradient lands first (preserves ADR
  014's intent that a non-beeping capacity warning not read as a dying disk).
- `alert` appears in both, as a noun.
- The Critical headline broadens from `disk health issue detected` to
  **`pool health issue detected`** -- genuinely cause-neutral, so it fits the
  non-disk Critical causes (`scrub_failed`, `computation_error`) and matches the
  "cause-neutral" wording ADR 014 already claims.
- The tier-specific action tail is preserved: Critical = `acknowledge and
  silence` (it beeps), Warning = `acknowledge` (no beep, nothing to silence).
- ASCII-only (`--`, no Unicode); satisfies `scripts/docs/check-output-ascii.py`.

## Scope boundary (confirmed by exploration)

The banner wording lives in exactly two production files. The alert
*notification* path -- `braid monitor`, the `alertCommand`, and every systemd
unit (`braid-alert.service`, `braid-alert-advisory.service`, `braid-beep.service`)
-- is **purely exit-code driven** (`cli/src/main.rs` maps severity to exit 1 vs
3; `modules/braid/` branches on the exit code). No banner text is embedded
there. **Out of scope, do not touch:** `monitor.rs`, `main.rs` exit codes,
`alert.rs` severity model, and all systemd units / `alertCommand` payloads.

## Part 1 -- Banner rename (production code)

Change the four literal strings; the surrounding match/control flow is unchanged.

- `cli/src/status.rs`, `format_status_human` (the `if report.alert_active` block):
  - Warning arm: `NOTICE -- capacity risk detected. Run 'braid ack' to acknowledge.\n`
    -> `WARNING alert -- capacity risk detected. Run 'braid ack' to acknowledge.\n`
  - Critical arm (the `_ =>` fallthrough): `ALERT -- disk health issue detected. Run 'braid ack' to acknowledge and silence.\n`
    -> `CRITICAL alert -- pool health issue detected. Run 'braid ack' to acknowledge and silence.\n`
  - Update the block's lead comment that references "the critical dying-disk
    line" only if wording drifts; the existing intent comment stays accurate.
- `cli/src/tui/view/mod.rs` (the `if alert_active` block, the `match alert_severity`):
  - Warning arm: ` NOTICE -- capacity risk detected. Run 'braid ack' to acknowledge. `
    -> ` WARNING alert -- capacity risk detected. Run 'braid ack' to acknowledge. `
  - Critical arm: ` ALERT -- disk health issue detected. Run 'braid ack' to acknowledge and silence. `
    -> ` CRITICAL alert -- pool health issue detected. Run 'braid ack' to acknowledge and silence. `
  - **Preserve the leading/trailing space** (layout padding) and the amber
    (Warning) / red (Critical) styling.
  - Update the comment `// Warning-only renders an amber NOTICE; any Critical
    cause ... renders the red ALERT.` -> "amber WARNING alert / red CRITICAL alert".

## Part 2 -- Tests (update assertions + stale vocabulary)

The new leading token (`CRITICAL alert` / `WARNING alert`) is the clean
discriminator; positive/negative assertions should key on it.

- `cli/src/status.rs` unit tests (4 functions): `status_human_warning_only_renders_notice_not_alert`,
  `status_human_critical_renders_alert_not_notice`,
  `status_human_scrub_failed_renders_alert_not_notice`,
  `status_human_mixed_severity_renders_alert`.
  - Update the 8 `contains`/`!contains` literals to the new strings.
  - **Rename the functions** to drop the dead `notice`/`alert` vocabulary, e.g.
    `..._renders_warning_not_critical`, `..._renders_critical_not_warning`,
    `..._mixed_severity_renders_critical`. Update each `// Intent / Why it exists
    / Scenario` preamble that says "NOTICE banner" / "ALERT -- disk health".
- `cli/src/tui/view/mod.rs` unit tests (2 functions):
  `tui_warning_only_renders_notice_banner`,
  `tui_critical_and_mixed_render_alert_banner`. Update the 4 assertions and
  rename to the new vocabulary.
- `tests/cli/braid-monitor-enospc.py`: `"NOTICE -- capacity risk detected" in human`
  -> `"WARNING alert -- capacity risk detected"`; `"ALERT -- disk health issue
  detected" not in human` -> `"CRITICAL alert" not in human`; update the subtest
  description that says "NOTICE (not ALERT) banner".

### Bare-`"ALERT"` VM assertions (re-key rule)

Several VM tests detect the banner by the uppercase word `"ALERT"`. The new
Critical string `CRITICAL alert -- pool health issue detected` contains **no**
uppercase `ALERT` substring, so every positive `assert "ALERT" in ...` would
**fail** and every negative `assert "ALERT" not in ...` would silently **pass**
even if a banner were wrongly shown (regression-guard loss). Re-key both
directions to a token present in the new output:

- **Positive assertions** (all are Critical-tier -- missing device, smartd,
  cleanup-pending; the only Warning cause is `EnospcRisk`) -> assert
  `"CRITICAL alert" in output`.
- **Negative assertions** (healthy / after-ack -- no banner expected) -> assert
  the shared banner discriminator is absent: `"alert --" not in output` (both
  new banners contain `alert --`; the live `warning: ENOSPC risk:` advisory and
  healthy text do not). Equivalently assert neither `"CRITICAL alert"` nor
  `"WARNING alert"` appears. This keeps the guard's teeth.

Apply to the files **not** otherwise covered above (none are currently named in
this plan):

- `tests/cli/braid-monitor.py`: degraded-pool positive (`:164`,
  `"ALERT -- disk health issue detected." in output` -> `"CRITICAL alert -- pool
  health issue detected." in output`, and update the `:162` "shows ALERT banner"
  subtest description); offline-status positive (`:308`); healthy negative
  (`:103`); after-ack negative (`:202`).
- `tests/cli/braid-smartd-alert.py`: positives at `:67` and `:97`; negatives at
  `:39` and `:86`.
- `tests/cli/monitor-hot-unplug.py`: positive at `:90`.
- `tests/cli/braid-ack-cleanup-pending.py`: positive at `:93`; negatives at
  `:53` and `:119`.

Update each assertion's failure-message string to match (e.g. "Expected
CRITICAL alert in degraded status").

## Part 3 -- Docs: `docs/commands/status.md` `### Alert banner` section

The section already documents both tiers, the severity split, the latch note,
and ENOSPC capacity risk in prose; the JSON `alert_causes` list already
enumerates all 6 discriminators (guarded by
`alert.rs#status_docs_list_every_alert_cause_discriminator`). So this part is the
narrow delta: the rename, plus two residual content gaps in this same section.
No JSON `alert_causes` edit is needed.

**3a. Banner-literal rename (in the two example blocks + split prose).** Replace
`ALERT -- disk health issue detected. ...` -> `CRITICAL alert -- pool health
issue detected. ...` and `NOTICE -- capacity risk detected. ...` -> `WARNING
alert -- capacity risk detected. ...`, and the bare `` `ALERT` `` / `` `NOTICE` ``
tokens in the "highest cause severity" prose. The Part 5 sweep flags these exact
sites (`status.md:84/93/98/99`); this is the doc half of the Part 1 rename.

**3b. Complete the cause-line example inventory.** `format_status_human` emits
six cause-line shapes; the example blocks currently show only four (btrfs device
errors, SMART, scrub failed, ENOSPC). Add the two missing shapes:

- `- missing device: <name> (devid <N>)`
- `- alert computation error: <detail>`

Put `missing device` in the Critical example block (alongside the existing btrfs
device-errors line). `alert computation error` is also Critical; add it where it
reads naturally. The device-named lines use `devid_to_name` (`cli/src/status.rs`),
which renders `<name> (devid <N>)` when the devid resolves to an operator name and
falls back to bare `devid <N>` otherwise -- so document the named form (matching
the existing `toshiba1 (devid 1)` example).

**3c. Add the Warning-tier ack distinction + ENOSPC post-ack nuance.** The split
prose states which tier wins but not how `ack` differs per tier. Add: **CRITICAL**
beeps, so `braid ack` acknowledges *and silences*; **WARNING** does not beep, so
`braid ack` acknowledges -- nothing to silence. Both tiers latch (the existing
latch sentence already covers this and stays). Then add the ENOSPC-specific
post-ack nuance: `braid ack` clears the latch (the banner goes away) and, when
the pool is still at risk, writes a snooze marker (`enospc-ack.json`,
`cli/src/ack.rs`) so the monitor re-fires `EnospcRisk` once the snooze window
elapses. The suppression is purely **time-based**:
`monitor.rs#evaluate_enospc_for_monitor` checks `baseline.is_snoozed(now)`, not a
margin/worsening comparison (pinned by `cmd_monitor_suppresses_enospc_within_snooze`)
-- so do **not** document a "risk got worse" re-fire condition; that was the old
margin-baseline design, replaced by the snooze timer. Meanwhile `braid status`
keeps printing the live `warning: ENOSPC risk: ...` advisory every run,
independent of the latch or snooze. For the user-facing doc, stay brief by
cross-linking the existing ENOSPC snooze prose (the `**ENOSPC risk on RAID1
pool.**` block already in `status.md`) rather than restating the mechanics; the
full re-fire condition list (marker absent/corrupt, pool-key mismatch,
unavailable FS UUID) is implementer context, not status.md copy.

## Part 4 -- ADR 014 (`docs/design/decisions/014-alerts.md`, status: Active)

- Line 33 (`docs/design/decisions/014-alerts.md`) quotes **both** old banner
  literals: the Critical `"ALERT -- disk health issue detected. Run 'braid ack'
  to acknowledge and silence."` and the Warning `"NOTICE -- capacity risk
  detected. Run 'braid ack' to acknowledge."`. Swap both quotes to the new
  strings. **Preserve the existing scoping:** the line already attributes
  "cause-neutral" to the *Critical* banner specifically ("Critical causes render
  the cause-neutral ... banner"), while the Warning banner names its domain
  (capacity). Keep that -- the Warning headline (`capacity risk detected`) is
  cause-specific (its sole cause is `EnospcRisk`) and must not be reworded to
  imply one universal cause-neutral banner. A full swap leaves no old-vocabulary
  hit (the Part 5 gate stays at zero).
- Note the banner now leads with the severity word + `alert`.
- Line 176's figurative "I acked but it still says ALERT" phrase is reworded by
  the Part 5 sweep (rule 2), not here.
- The severity-tier paragraph that says status/TUI "render a distinct
  lower-urgency banner for a Warning-only alert": name the scheme -- Critical =
  `CRITICAL alert -- ...`, Warning = `WARNING alert -- ...`; the Warning tier is
  the quiet word, no beep.

## Part 5 -- Tracked-file vocabulary sweep (authoritative catch-all)

The old vocabulary lives in comments, intent text, assert messages, and bare
`"ALERT"` assertions -- not just the banner literals -- so a doc-only or
literal-only grep would leave stale `NOTICE`/`ALERT` wording behind *and* leave
broken tests. Run a tracked-file sweep over code, tests, docs, and the module,
excluding the generated book, and classify **every** hit (see disposition rules
below):

```
# Whole-word ALERT / NOTICE (case-sensitive), plus the old Critical headline phrase.
git grep -n -w -E "ALERT|NOTICE" \
  -- cli/src tests docs README.md modules ':!docs/book'
git grep -n -F "disk health issue detected" \
  -- cli/src tests docs README.md modules ':!docs/book'
```

**Tooling gotcha (verified):** do **not** use `\b(ALERT|NOTICE)\b` with
`git grep -E` -- POSIX ERE ignores `\b`, so that pattern returns **zero hits**
and gives false confidence. Use `-w` for whole-word matching (the `-P` PCRE
engine with `\b` works too). The `-w` flag is what excludes false positives like
`POLLFREQALERT` in the UPS docs (verified: zero matches there). The
whole-word sweep currently returns ~83 hits; the phrase sweep catches the old
Critical headline.

`capacity risk detected` is deliberately **not** swept -- it survives verbatim in
the new `WARNING alert -- capacity risk detected` string, so sweeping for it
would flag valid new output. Both whole words (`ALERT`, `NOTICE`) and the old
phrase (`disk health issue detected`) are absent from the new strings
(`WARNING alert -- capacity risk detected`, `CRITICAL alert -- pool health issue
detected`), where `alert` appears only lowercase.

### Disposition rules (apply to every hit)

1. **Banner-literal references** (the production strings, the doc example blocks,
   assert literals, and assert-message strings like "must render the ALERT
   banner") -> rewrite to the new vocabulary / new strings.
2. **Figurative / mental-model references** that describe what the operator sees
   on screen ("I acked but it still says ALERT", "Operator misses the first
   ALERT") -> reword to the new banner word ("...still says CRITICAL alert") or
   genericize ("...still shows an alert") so the gate can reach true zero. These
   are not banner literals but they must not leave the screen vocabulary stale.
3. **Comment / intent / Scenario text** -> rewrite to the new vocabulary.

Beyond the production strings and assertions covered in Parts 1-2, this sweep
catches hits those targeted parts don't enumerate -- handle each per the rules
above:

- `cli/src/alert.rs` -- the `scrub_failed_severity_is_critical` test's
  `// Why it exists` preamble ("status renders NOTICE instead of ALERT").
- `cli/src/tui/view/mod.rs` -- the production comments at the banner block
  ("renders a non-beeping amber NOTICE", "renders the red ALERT") and the two
  TUI banner-test preambles that say NOTICE/ALERT.
- `cli/src/status.rs` -- the four banner-test `// Intent / Why it exists /
  Scenario` preambles that reference the NOTICE/ALERT banner.
- `cli/src/ack.rs` -- the comment "I acked but it still says ALERT" state
  (rule 2: figurative).
- `docs/commands/status.md` -- the prose "the `ALERT` banner" (Part 3a already
  reworks this section; confirm no stale token survives).
- `docs/design/decisions/014-alerts.md` -- line 33's banner-literal quotes
  (Part 4) and the line-176 "I acked but it still says ALERT" figurative quote
  (rule 2).
- `tests/cli/braid-monitor.nix` -- the file header comment "shows the ALERT
  banner".
- `tests/cli/braid-monitor.py` -- the figurative Scenario comments ("Operator
  misses the first ALERT", "I acked but it still says ALERT") in addition to the
  assertions handled in Part 2.
- `tests/cli/braid-monitor-enospc.nix` and `tests/cli/braid-monitor-enospc.py`
  -- file-level header comments describing "the NOTICE banner".

Re-run both sweeps after editing; they should return **zero hits**,
unconditionally (Part 4 fully replaces the 014:33 banner quote, so there is no
preserved old-vocabulary quote to except). `docs/commands/ack.md` is left
untouched: its prose ("Acknowledges active alerts and silences the PC speaker
beeper", `docs/commands/ack.md`) describes the ack action in its own words and
never quotes the banner literal, so the rename does not reach it -- and it
contains no `ALERT`/`NOTICE` whole-word or old-phrase hit.

## Verification

1. **Rust unit tests:** `just test-rust` -- the 6 renamed banner tests in
   `status.rs` + `tui/view/mod.rs` pass against the new strings.
2. **ASCII gate:** `scripts/docs/check-output-ascii.py` (or its `just` recipe)
   stays green -- new strings are ASCII.
3. **VM tests:** run **all five** affected NixOS VM tests -- `braid-monitor`,
   `braid-monitor-enospc`, `braid-smartd-alert`, `monitor-hot-unplug`, and
   `braid-ack-cleanup-pending` (the latter three carry bare-`"ALERT"` assertions
   re-keyed in Part 2; check `flake.nix` `checks` / `justfile` for the exact
   recipe; they run on `nix.linux-builder`). Confirm the degraded/missing/smartd/
   cleanup-pending cases show `CRITICAL alert -- pool health issue detected` and
   the ENOSPC case shows `WARNING alert -- capacity risk detected` and not the
   Critical banner.
4. **Docs build:** `just docs-build` -- mdbook linkcheck passes (new cross-links
   in the Alert banner section resolve).
5. **Manual smoke:** build the CLI; on an ENOSPC-at-risk pool with the monitor
   having latched the cause, `braid status` shows the `WARNING alert` banner
   stacked above the live `warning: ENOSPC risk: ...` advisory (no `warning:`
   word collision); a degraded pool shows the `CRITICAL alert` banner. Spot-check
   the TUI banner colors (amber vs red).

## Out of scope

- No changes to exit codes, `AlertSeverity`, the alert latch, `braid monitor`,
  `alertCommand`, or any systemd unit -- the rename is display-only.
- No new alert causes or severity tiers.
- **No JSON `alert_causes` change.** The rename touches only the human banner
  text, not the snake_case `type` discriminators, and all 6 are already
  documented in `status.md` and guarded by
  `alert.rs#status_docs_list_every_alert_cause_discriminator`; that test stays
  green with no action.

## Implementation notes

- Added `docs/commands/status.md` to the crane source filter so
  `alert.rs#status_docs_list_every_alert_cause_discriminator` can read its
  compile-time doc fixture during the Nix `braid-cli` package check that gates
  the affected VM tests.
