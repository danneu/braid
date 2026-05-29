# Collapse the unreachable LUKS `Damaged` header state

## Context

The docs-accuracy sweep (`plans/impl/2026-05-29-docs-luks-probe-model-accuracy.md`)
established, against pinned cryptsetup source, that braid's `isLuks` / `luksUUID`
/ `luksDump` probes all gate on the **same** `crypt_load`. The behavioral
consequence it documented but deliberately did not act on:

- Genuine LUKS2 metadata damage fails `isLuks` -> `LuksHeaderState::Unreadable`.
- `LuksHeaderState::Damaged` (`isLuks` ok + `luksDump` fail) is reachable **only**
  from a transient fault between the two probe invocations -- never from real
  corruption. Its dedicated `cryptsetup repair` guidance is therefore advice that
  fires only on a transient blip, where repair is the wrong remedy.

So `Damaged` is dead diagnostic surface: it fans out across 6 enums, a guidance
function, the `git grep 'Damaged' -- cli/src` set (~80 hits), and several
doc/VM-test sites that the sweep just re-documented truthfully. This plan
**verifies the implementor's three follow-up recommendations** and specifies the
**ideal full collapse**: delete the `Damaged` distinction everywhere, leaving
"unreadable" as the single conservative header-failure state.

Behavior for real corruption is unchanged (it already routed to `Unreadable`); the
only user-visible deltas are (1) the never-genuinely-hit `Damaged` label and its
`cryptsetup repair` text disappear, and (2) a transient second-probe blip now
reports "unreadable" instead of "damaged" -- the correct conservative answer. This
is the behavioral pass the sweep handed off; doing it now pays down the churn the
sweep warned about (it deletes most of what the sweep just re-documented).

## Verification of the three follow-up recommendations

**#1 -- collapse, option (a): CONFIRMED CORRECT.**
- `Damaged` is produced at one site: `probe_luks_header`'s `luksDump`-fail arm
  (`cli/src/luks.rs:730`). `probe_config_disk` (`cli/src/probe.rs`) does **not** call
  `probe_luks_header`; its own `luksDump`-for-version call is the separate
  LUKS2-invariant gateway and is correctly **kept**.
- `luks_header_damaged_guidance` (`luks.rs:805-819`) + its `cryptsetup repair` text
  are reachable only via `Damaged` (callers `doctor.rs:419`, `mount.rs:479`). There
  is no `CryptsetupRepair` command -- guidance text only.
- Option (b) is infeasible: cryptsetup exposes no non-mutating "one copy is bad"
  probe; by the time `isLuks` fails the header is unrecoverable from on-disk copies.
  **Pursue (a).**

**Open question the implementor left ("optionally fold a soft `cryptsetup repair`
line into the unreadable guidance"): SETTLED -- do NOT fold.** An existing VM
invariant forbids it: `tests/cli/braid-doctor.py:329-330` asserts the unreadable
path must **not** contain "cryptsetup repair" ("Classified as unreadable (severe),
not damaged -- must NOT recommend repair"). Remove the repair guidance outright;
keep `luks_header_unreadable_guidance` (off-system backup) as the single remedy.

**#2 -- end-user docs: CONFIRMED but UNDER-SCOPED.** The cited `doctor.md:100` does
not exist (only `doctor.md:71`); the real doc/string surface is larger -- see E/G.

**#3 -- rename the gateway test: CONFIRMED, name accurate.**
`unlock_damaged_luks2_metadata_fails_at_gateway` (`mount.rs:3351`) mocks `luksUUID`
succeeding + `luksDump` failing, asserting `probe_config_disk` hard-errors before
unlock. It exercises the **version-check gateway**, not `probe_luks_header`, so it
**survives untouched** -- pure rename to `unlock_gateway_rejects_luksdump_failure_before_unlock`.

## Review findings folded in (from the rejected v1)

1. **Medium -- prove the second probe is gone (structural invariant).** `MockRunner`
   tolerates unused mocks (`cmd.rs:1454` returns `Err(MissingMock)` only for
   *unmatched* calls), so `probe_luks_header_ok` (`luks.rs:3550`) -- which seeds a
   `luksDump` mock -- would still pass if the redundant probe regressed. Fix in A.
2. **Medium -- exhaustive test disposition.** v1 listed only `fn`s named `*damaged`;
   several **multi-state fixtures** embed the variant as one row
   (`mount.rs:1643/1952/2010`, `doctor.rs:3404/3461`, `status.rs:1985`,
   `tui/view/mod.rs:2359`). Section D now dispositions **every** `git grep 'Damaged'`
   hit.
3. **Low -- surviving comments still describe the two-probe model.** `explain_open_failure`
   (`mount.rs:453`), `status.rs:1095-1105`, `tui/probe.rs:344/418` etc. Section E
   sweeps them, distinguishing `probe_luks_header` (now isLuks-only) from
   `probe_config_disk`/`PresentLuks` comments (which legitimately still run both).

## The change

### A. Core enum + probe + guidance -- `cli/src/luks.rs`
- Delete the `Damaged` variant (`683`) from `LuksHeaderState`.
- In `probe_luks_header` (`718-733`), delete the entire `luksDump` match block;
  `isLuks` success returns `Ok`. Result: `Err -> ProbeFailed`, `isLuks` non-zero
  `-> Unreadable`, else `-> Ok`.
- Rewrite the docstrings that describe the two-probe model: `probe_luks_header`
  (`690-717`), the enum "Terminology contract" (`661-668`), the `Ok` variant doc
  (`671-674`). Keep the true points: reads the raw block device not the mapper; not
  strictly read-only (`crypt_load` may auto-recover). Drop the "open design question"
  paragraph -- now resolved.
- Delete `luks_header_damaged_guidance` (`805-819`, doc + fn). Keep
  `luks_header_unreadable_guidance` (`742`) verbatim -- do **not** add repair text.
- **Structural invariant (review finding 1):** rewrite the `probe_luks_header_ok`
  test (`3550`) to seed **only** the `is_luks_ok` mock and assert `Ok`, mirroring the
  existing `probe_luks_header_unreadable_when_is_luks_fails` (`3576`) "deliberately
  absent luksDump mock" pattern. Then a re-added `luksDump` call hits `MissingMock`
  -> `ProbeFailed` -> the test fails. Update its preamble to say so.

### B. Compiler-guided variant deletion
Delete the variant and every arm that handles it (drop the **token** where the arm
is shared with `Unreadable`; delete the **whole arm** otherwise). After each enum
deletion the compiler's non-exhaustive-match errors enumerate the rest; this is the
expected set:

| Variant (def) | Producer | Consumers |
|---|---|---|
| `doctor::DiskState::LuksHeaderDamaged` (`314`) | `351` | `416-421` arm; `header_damaged` vec at `393`, sum at `429`, block at `464-470` |
| `mount::MissingReason::LuksHeaderDamaged` (`56`) | via C | `63` drop token; `87` delete arm |
| `mount::ProbeEvent::DiskLuksHeaderDamaged` (`122`) | via C | `148-152` arm |
| `status::DiskStatus::LuksHeaderDamaged` (`182`) | `1094` (see C) | `193` Display; `1395-1397`; `1444-1447`; `1462` drop token |
| `tui::model::UnpooledDiskRender::LuksHeaderDamaged` (`281`) | via C | `tui/view/mod.rs:832` |

In `mount.rs:477-480` delete the `Damaged` arm of `explain_open_failure` (last caller
of the deleted guidance) and fix its docstring (`443-464`: "four arms" -> three).

### C. `PresentNotLuks` re-probe: three sites, two treatments
All three re-probe `probe_luks_header` after `probe_config_disk` returns
`PresentNotLuks`. They do **not** simplify the same way -- this is the most
error-prone part:

- **`mount.rs::plan_open_pool` (`259-277`) and `tui/probe.rs::probe_pool_for_tui`
  (`417-428`): remove the probe entirely.** Both already map everything except
  `Damaged` to `Unreadable` (`_ => Unreadable`), so once `Damaged` is gone the probe
  is a constant. Push `LuksHeaderUnreadable` (and `ProbeEvent::DiskLuksHeaderUnreadable`
  for mount) directly; rewrite the inline comments. Behavior-preserving and
  VM-confirmed (`braid-unlock.py:538` expects "raw: LUKS header unreadable", `:545`
  expects no "damaged"). Bonus: stops a needless `isLuks`+`luksDump` pair -- each a
  possible auto-recovery write -- during read-only planning.
- **`status.rs::build_disk_reports` (`1086-1109`): KEEP the probe; delete only the
  `Damaged` arm (`1094`).** Unlike the other two, status meaningfully maps
  `Unreadable -> LuksHeaderUnreadable`, `Ok -> Unknown` (don't overclaim a transient
  blip), `ProbeFailed -> Unknown`. Removing the probe here would regress the
  "PresentNotLuks + clean re-probe -> Unknown" behavior pinned by the surviving test
  at `5028`. Reword the `1095-1105` comment to drop "overclaiming Damaged" while
  keeping the Ok->Unknown rationale.

### D. Test dispositions (exhaustive -- every `git grep 'Damaged' -- cli/src` test hit)

**Delete (sole purpose is the Damaged path; each has a 1:1 `_unreadable` sibling
that preserves coverage):**

| Test (file:line) | Sibling that preserves coverage |
|---|---|
| `luks.rs:3598` `probe_luks_header_damaged_when_dump_fails` | `3576` unreadable |
| `luks.rs:3692` `luks_header_damaged_guidance_interpolates...` | n/a (fn deleted) |
| `doctor.rs:3213` `summarize_warn_luks_header_damaged` | `3164` unreadable |
| `mount.rs:1559` `format_degraded_refused_damaged_includes_disk_name_and_reason` | `1391` unreadable |
| `mount.rs:1603` `format_degraded_refused_damaged_includes_doctor_footer` | `1584` unreadable |
| `mount.rs:2553` `explain_open_failure_damaged_overrides_fallback` | `2507` unreadable |
| `status.rs:2865` `status_verbose_luks_header_damaged_disk` | `2800` unreadable |
| `status.rs:4980` `build_disk_reports_present_not_luks_damaged_maps_to_luks_header_damaged` | `4944` unreadable + `5028` inconsistent->Unknown |
| `tui/probe.rs:2410` `unpooled_disk_present_not_luks_damaged_classified_correctly` | `2338` unreadable |

**Rename (survives -- recommendation #3):** `mount.rs:3351`
`unlock_damaged_luks2_metadata_fails_at_gateway` -> `unlock_gateway_rejects_luksdump_failure_before_unlock`.

**Re-point to Unreadable (no sibling; keep the unique coverage):**

| Test | Action |
|---|---|
| `recover.rs:18345` `..._excludes_damaged_header_disk` | Drop the now-unused `isLuks`/`luksDump` mocks for `virtio-old` (keep the `luksUUID`-fail mock that makes it `PresentNotLuks`); change assertion "LUKS header metadata damaged" -> "LUKS header unreadable"; simplify preamble; rename to `..._excludes_unreadable_header_disk`. |
| `tui/probe.rs:2338` `unpooled_disk_present_not_luks_unreadable_classified_correctly` | After C removes the tui probe, drop the now-unused `isLuks` mock; keep the `luksUUID`-fail mock; assertion (Unreadable render) and intent survive; update preamble to "PresentNotLuks renders Unreadable" (no longer "refines via probe"). |

**Re-point one fixture row inside a multi-state test (delete the row, NOT the test --
preserve footer/aggregation/render coverage):**

| Test | Action |
|---|---|
| `mount.rs:1643` `format_degraded_refused_mixed_includes_doctor_footer_once` | disk3 `LuksHeaderDamaged` -> `LuksHeaderUnreadable`; assertion "disk3: LUKS header metadata damaged" -> "disk3: LUKS header unreadable"; fix comment `1640`. (footer-once coverage survives.) |
| `mount.rs:1952` `render_probe_events_formats_mixed_probe_result` | Remove the `DiskLuksHeaderDamaged` disk3 row (`1963-1965`) and its expected line (`1980`). |
| `mount.rs:2010` `probe_event_to_preview_note_preserves_byte_format` | Remove the `DiskLuksHeaderDamaged` case (`2031-2035`). |
| `doctor.rs:3404` `summarize_declared_disks_fail_dominates_warn_level_problems` | disk2 `LuksHeaderDamaged` -> `LuksHeaderUnreadable` (any warn state; fail-dominates coverage is the point); fix comment `3402`. |
| `doctor.rs:3461` `summarize_mixed_states_reports_all` | disk3 `LuksHeaderDamaged` -> `DiskState::ProbeFailed("...".to_owned())` so the test still aggregates **distinct** categories (Missing/Unreadable/ProbeFailed = 3/4); fix comment `3469`. |
| `status.rs:~1985` (the multi-row render/JSON fixture holding the `damaged` disk5 `DiskReport`) | Remove the disk5 `DiskReport` (`1985-1994`) and any disk5 assertion; the `unreadable` (disk4) + `mismatch` (disk6) rows preserve coverage. Confirm the enclosing `fn` during impl. |
| `tui/view/mod.rs:2359` `unpooled_disk_status_cell_renders_each_variant` | Remove the `delta` `LuksHeaderDamaged` row (`2368`) and its assertion. |

**Keep, comment-only update (premise survives, comment names Damaged):**
`status.rs:5028` `..._inconsistent_falls_back_to_unknown` (comment `5014-5023` "rather
than LuksHeaderDamaged" / "labelling it Damaged"); `status.rs:4908`, `4938`, `2843`
comments in surviving tests. Reword to the post-collapse model.

### E. Surviving comment/doc-comment sweep (review finding 3)
Reword every remaining comment that asserts the two-probe / `Damaged` model.
**Distinguish:** comments about `probe_luks_header` -> rewrite to the isLuks-only
model; comments about `probe_config_disk` / `PresentLuks` (which still run
`luksUUID`+`luksDump`) stay correct -- do not touch them. Sites: `luks.rs:666/711`;
`mount.rs:449/453` (explain_open_failure doc: "Ok -> crypt_load + dump succeeded" ->
"Ok -> crypt_load validated the header (isLuks ok)"); `status.rs:1095-1105`;
`tui/model.rs:278-279/304`; `tui/probe.rs:344`; `types.rs:518` ("Unreadable/Damaged"
-> "Unreadable"). Final gate: `git grep -n 'Damaged' -- 'cli/src/'` returns **zero**.

### F. End-user docs
- `docs/commands/status.md:165` -- delete the `LUKS HEADER DAMAGED` table row.
- `docs/commands/status.md:284` -- drop `luks-header-damaged` from the `status` JSON
  enumeration; set it to match the post-change `Display`: `present`, `missing`,
  `luks-header-unreadable`, `luks-uuid-mismatch`, `unknown` (also fixes a pre-existing
  omission of `luks-uuid-mismatch`).
- `docs/commands/doctor.md:71` -- "unreadable or damaged LUKS header" -> "unreadable
  LUKS header".
- `docs/commands/unlock.md:83` -- "physically absent or with a damaged LUKS header"
  -> "...with an unreadable LUKS header".
- `docs/internals/luks-unlock.md:159` -- delete the `Damaged -- emit cryptsetup
  repair guidance` bullet; the list becomes `Unreadable` / `Ok` / `ProbeFailed`.
  Leave line 172 ("wiped or damaged header") -- physical reality that surfaces as
  `Unreadable`; the re-probe rationale (`167-172`) still holds.

### G. VM-test comments -- `tests/cli/braid-unlock.py`
- `:696` drop "or cryptsetup repair".
- `:702` "the existing 'LUKS header damaged' + degraded-refused path" -> "unreadable"
  (already stale: `PresentNotLuks` reports unreadable).
- `:707-708` "explain_open_failure_* (5 tests) ... (Unreadable/Damaged/Ok/ProbeFailed)"
  -> 4 tests / (Unreadable/Ok/ProbeFailed).
- `:543-546` the `"LUKS header damaged" not in output` assertion stays valid (keep);
  optionally drop its "renamed status line" historical framing.
- `tests/cli/braid-doctor.py` -- **no change.** Confirming evidence: `:315` proves real
  corruption fails `isLuks`; `:329` pins the no-repair invariant. Cite, do not edit.

## Verification

- `just test-rust` -- the unit suite must pass after the deletions/renames/re-points.
  The compiler's non-exhaustive-match errors are the checklist for B/C.
- **Structural invariant:** confirm the rewritten `probe_luks_header_ok` seeds no
  `luksDump` mock, so a regression re-adding the second probe fails (MissingMock ->
  ProbeFailed != Ok).
- **Completeness gate:** `git grep -n 'Damaged' -- 'cli/src/'` returns zero. Then the
  doc/cross-tree sweep returns only the intentional negative-assertion lines and the
  btrfs "damaged" in `troubleshooting.md`:
  ```
  git grep -ni 'luksheaderdamaged\|luks-header-damaged\|luks header damaged\|header metadata damaged\|cryptsetup repair\|damaged luks\|damaged header\|luks_header_damaged' \
    -- ':!reference/' ':!plans/'
  ```
- VM tests on the touched paths: `just test-vm braid-unlock braid-doctor` (the
  `raw`-member degraded-refusal at `braid-unlock.py:529-546` and the real-header
  corruption at `braid-doctor.py:300-339` are the end-to-end proofs).
- `plan_open_pool` is mount-path -- broad blast radius per AGENTS.md. After the
  focused runs pass, hand back for the user's full `just test-vm` rather than
  auto-running the whole suite.
- `cargo build` to confirm no malformed docstring/attribute. Do **not** run a formatter.
- `mdbook build docs` -- four `docs/` files change; linkcheck must stay green.

## Sequencing

One focused change/PR clears #1/#2/#3 plus the missed surface, as the implementor
recommended. The collapse (A/B/C) is the substance; tests (D), comments (E), docs
(F), and VM-test comments (G) ride along so code, docs, and tests land in lockstep
and `mdbook` / `just test-rust` validate them together.

## Implementation notes

- Section E's grep-driven enumeration keyed on the capital-`Damaged` token, so
  it missed three accuracy fixes the collapse forces. Handled them in the same
  spirit: (a) `doctor.rs` `LuksUuidMismatch` variant doc claimed `luksDump` ran
  -- but doctor's `classify_luks_identity` now reaches the UUID compare after
  `isLuks`+`luksUUID` only, so dropped the `luksDump` clause; (b) `tui/model.rs`
  `LuksHeaderUnreadable` variant doc and the `tui/view/mod.rs`
  `unpooled_disk_status_cell_renders_each_variant` doc still described the
  removed probe-refinement / "header damaged" vocabulary, reworded to the
  direct PresentNotLuks->Unreadable model.
- Two `!human.contains("LUKS HEADER DAMAGED")` assertions (`status.rs`
  `status_verbose_unknown_disk` and the duplicate-unpooled-row test) used the
  all-caps rendered label, so the plan's `git grep 'Damaged'` set did not list
  them. After the collapse no code path can emit that label, so the assertions
  are vacuous; removed them (keeping the live `LUKS HEADER UNREADABLE` /
  `UNKNOWN` guards), matching the plan's tui/view row-removal pattern.
- Removing the second probe and its tests orphaned the `luks_dump_text_ok` /
  `luks_dump_text_fail` helpers in `luks.rs`'s test module (no remaining
  callers); deleted both to keep `cargo check --tests` warning-free.
- `status.rs` `build_disk_reports_present_not_luks_inconsistent_falls_back_to_unknown`
  was scoped comment-only by section D, so its now-unused `luksDump`-ok mock was
  left in place (MockRunner tolerates unused mocks); only the comment was
  reworded to the isLuks-only model.

## Follow Up

- `cli/src/status.rs` `build_disk_reports_present_not_luks_inconsistent_falls_back_to_unknown`
  still seeds a `CryptsetupLuksDumpText` ok mock that the collapsed
  `probe_luks_header` (isLuks-only) no longer consumes. Harmless (tolerated
  unused mock) but dead; a future cleanup can drop it.
