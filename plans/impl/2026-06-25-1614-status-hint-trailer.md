# Plan: Centralize the "see `braid status`" operator-hint trailer

## Context

A review finding flagged `cli/src/remove.rs:567` for telling the operator to
run `braid status` "to see the missing disk's name and device IDs" -- the noun
"device IDs" is ambiguous against decision 024's identity vocabulary and does
not match what `braid status` actually prints. Verification showed the cited
line is one instance of a wider problem: the trailing sentence
`Use \`braid status\` to see ...` is **copy-pasted 13 times across 8 files** and
has already drifted into:

- **three wordings** -- "the missing disk's name", "the missing disk's name and
  device IDs", "device names and IDs", "device IDs";
- **quote-style drift** -- ``` `braid status` ``` (backticks) vs
  `'braid status'` (single quotes) at `replace.rs:1909` and
  `remove_missing.rs:315`;
- **a latent plural bug + missing period** at `doctor.rs:892`, which says "the
  missing disk's **name**" (singular) even though that check reports `N` missing
  devids, and omits the trailing period the other sites have.

The noun "device IDs" is also genuinely misleading: `braid status` prints the
literal token `devid` (`status.rs#display`, e.g. `format!("devid {id}")`) **and**
a separate by-id hardware path on the `Device:` row, so "device IDs" reads as
the by-id path -- the opposite of the btrfs devid the follow-on
`remove-missing --missing-id <devid>` / `replace --missing-id <devid>` consume.

braid already has a module whose entire purpose is to stop this class of drift:
`cli/src/repair_hint.rs` -- *"Central missing-device replace shape so operator
hints do not drift."* It owns `missing_replace_command`, the `--missing-id`
cross-check phrasing, and the hot-unplug guidance. The `braid status` trailer is
a member of that same operator-hint family that leaked out of the module. **The
fix is to finish what `repair_hint` was created to do**: give the trailer one
home, fix the noun to `devid`, make it plural-correct, and route every site
through it. Outcome: the trailer becomes un-driftable, `doctor.rs`'s bugs are
fixed for free, and the vocabulary matches decision 024 and `braid status`
output.

Per AGENTS.md ("reach for the ideal, robust, simple, most correct solution --
regardless of scope") this is the full-dedup pass, confirmed with the user.

## Design: a `repair_hint` trailer family

Add three `pub(crate)` helpers to `cli/src/repair_hint.rs`, each returning the
bare sentence (capital `Use`, trailing period, no surrounding whitespace). The
noun is **`devid`/`devids`** -- the token `braid status` prints and decision
024's base vocabulary. Plurality follows the `missing_count == 1` idiom already
used right next to every call site (`if missing_count == 1 { "" } else { "s" }`).

| Helper | Signature | count == 1 | count != 1 |
| --- | --- | --- | --- |
| `see_missing_names_in_status` | `(missing_count: u64) -> String` | `` Use `braid status` to see the missing disk's name. `` | `` Use `braid status` to see the missing disks' names. `` |
| `see_missing_names_and_devids_in_status` | `(missing_count: u64) -> String` | `` Use `braid status` to see the missing disk's name and devid. `` | `` Use `braid status` to see the missing disks' names and devids. `` |
| `see_devids_in_status` | `() -> String` | n/a -- fixed: `` Use `braid status` to see which devids are missing. `` | |

Variant C is count-free and phrased as a relative clause ("which devids are
missing") so it is naturally plural-safe and points exactly at the valid
`--missing-id` targets (the rejection sentence preceding it already states what
was wrong).

Conventions to match (confirmed against the module):

- **Doc comment**: intent-first `///` on each helper saying *why* it exists at
  the boundary, not what the signature says. State that "devid" is the literal
  `braid status` token (disambiguating from the by-id path on the same row) and
  that plurality exists to retire the `doctor.rs` singular drift.
- **Tests**: one unit test per helper, each with the three-line
  `// Intent: / // Why it exists: / // Scenario:` preamble and exact-string
  `assert_eq!`. Cover **both** singular and plural for the two count-aware
  helpers; the fixed string for `see_devids_in_status`.
- **ASCII only**: plain `'`/`"`, `--`, `...` (enforced by
  `scripts/docs/check-output-ascii.py`).
- **Caller idiom**: `use crate::repair_hint;` then
  `repair_hint::see_*_in_status(...)`; callers own the preceding separator and
  any wrapping punctuation (e.g. `remove.rs`'s closing paren).

## Call-site migration

Replace each literal trailer with an interpolated helper call. Files and the
count argument to pass:

**Variant A -- `see_missing_names_in_status(...)`**

- `cli/src/add.rs#format_add_missing_devices_warning` -- pass `missing_count`.
- `cli/src/replace.rs` (live-replace missing-device guard, ~:1855) -- pass
  `pool.missing_count`.
- `cli/src/pool.rs#device_remove_error` (`RemoveContext::Missing`, ~:336) -- no
  count in scope and the surrounding prose is singular; pass `1`.
- `cli/src/doctor.rs` `pool_missing_devices` warn (~:892) -- pass the in-scope
  missing count `n`. This **fixes the plural bug** ("disks' names" when `N>1`)
  and the helper's trailing period **fixes the missing period**.

**Variant B -- `see_missing_names_and_devids_in_status(...)`**

- `cli/src/remove.rs` (disk-not-found-with-missing branch, :567) -- pass
  `pool.missing_count`. Keep the existing wrapping `()`; helper output ends in
  `.` so the result stays `...devids.)`.
- `cli/src/preflight.rs#check_no_missing_devices` (:326) -- pass `missing_count`.
- `cli/src/remove_missing.rs` (2-disk RAID1 floor guard, :429) -- pass
  `pool.missing_count` (here always `1`).

**Variant C -- `see_devids_in_status()`**

- `cli/src/replace.rs` (bad `--missing-id` rejection, :1909) -- also retires the
  single-quote `'braid status'` drift.
- `cli/src/remove_missing.rs#...` (bad `--missing-id` rejection, ~:315) -- same
  single-quote fix.

## Test + docs updates

**Pinning tests** (exact-string assertions that must move to the new wording):

- `cli/src/add.rs` unit test (~:10590) -- Variant A, count 1.
- `cli/src/remove_missing.rs` unit test (~:1828,
  `plan_remove_missing_rejects_wrong_missing_id_from_pool_state`) -- Variant C.
- `tests/cli/braid-add-warnings.py` (two assertions, ~:97 and ~:247) -- Variant
  A, count 1.

**New behavioral pin for the `doctor` plural/period fix**: the plan claims to
fix `doctor.rs`'s singular-when-`N` bug and missing period. The helper unit
tests pin the rendered strings in isolation but **cannot catch a wrong-count
wiring** at the call site, and the residual grep would still pass. Pin the claim
at the `doctor` boundary by extending the two existing tests in
`cli/src/doctor.rs` (both already assert on `check.message` via `.contains`):

- `pool_missing_devices_plural_warns_with_single_replace_command` (~:5900, two
  missing devids) -- assert `check.message.contains("Use `braid status` to see the missing disks' names.")` (plural form **with** trailing period). This pins
  both the count wiring (`n > 1` must render plural) and the period.
- `pool_missing_devices_warns_with_replace_recommendation` (~:5834, the singular
  path) -- assert `check.message.contains("Use `braid status` to see the missing disk's name.")` so the singular trailer + period are pinned too.

Both are substring checks on rendered output: structure-insensitive (survive
incidental message rewording) but behavioral (catch a miswired count or a
dropped period).

**Docs**: three command-doc references independently carry the ambiguous
"device IDs" lookup phrasing for `--missing-id` -- not the trailer sentence, but
the **same noun the refactor retires**. Update them to `devid`/`devids` so the
docs do not reintroduce the ambiguity the CLI now removes:

- `docs/commands/status.md` (#when-to-use-it, ~:14): "To find device IDs needed
  by other commands (`--missing-id`)" -> "To find devids needed by other
  commands (`--missing-id`)".
- `docs/commands/replace.md` (#related-commands, ~:131): "find device IDs and
  see which disks are missing" -> "find devids and see which disks are missing".
- `docs/commands/remove-missing.md` (#related-commands, ~:98): "find missing
  device IDs" -> "find missing devids".

Deliberately **out of scope** (so the grep gate stays scoped to the three files
above, not a blanket docs sweep): the singular field gloss at `status.md` (~:480,
"`devid`: btrfs device ID as a number") *defines* the term and is unambiguous;
`docs/guides/power-management.md` (~:147, ~:238) uses "device IDs" for PCI
vendor/device IDs, a different concept; `docs/commands/unlock.md` (~:73, "live
btrfs device IDs") is an enrichment description, not a `--missing-id` lookup
instruction (optional same-noun tidy-up, not required by this refactor).

## Verification

1. **Unit tests**: `just test-rust` -- exercises the three new `repair_hint`
   tests plus the updated `add.rs` and `remove_missing.rs` pins.
2. **ASCII guard**: `python3 scripts/docs/check-output-ascii.py` (or its `just`
   recipe) -- the new strings are ASCII.
3. **No residual drift** -- separate the strings that must *vanish* from the
   strings that now have a legitimate canonical home (a blanket "all greps return
   zero" gate false-fails on both comments and the new canonical wording):
   - **Must be zero** (true drift, no legit home post-refactor):
     `rg "device IDs|device ids|device names and IDs" cli/src` -> zero. Reaching
     zero includes a one-word tidy-up of the lone non-output hit -- a test
     doc-comment at `cli/src/alert.rs` (~:2002, "device IDs" -> "devids").
   - **Do NOT gate on zero** (these legitimately persist, so a zero-gate would
     false-fail):
     - "the missing disk's name" / "missing disks' names" now lives in
       `repair_hint.rs` (helper bodies + unit tests) **and** in the
       rendered-output pinning tests (`add.rs` ~:10590, the `doctor.rs` ~:5834
       assertion). It is the canonical wording, not drift.
     - single-quoted `'braid status'` legitimately remains in `cli/src/main.rs`
       (~:346, the `--missing-id` clap help -- already says "btrfs devid", and
       plain-text quotes are the clap-help convention) and `cli/src/preflight.rs`
       (~:62, an unrelated message). The two single-quoted *trailer* sites this
       refactor converts (`replace.rs:1909`, `remove_missing.rs:315`) are proven
       converted by the "device IDs -> zero" gate plus the delegation check below.
   - **Delegation confirmed positively** (the real anti-drift invariant -- every
     call site routes through a helper rather than inlining the sentence):
     `rg "see_missing_names_in_status|see_missing_names_and_devids_in_status|see_devids_in_status" cli/src`
     lists all nine production call sites (4 Variant A + 3 B + 2 C) plus the
     definitions/tests in `repair_hint.rs`.
4. **Docs in sync** (gate scoped to the three edited references, *not* a blanket
   docs sweep -- PCI "device IDs" in `power-management.md` and the `unlock.md`
   enrichment line are legitimate and must survive):
   `rg "device IDs" docs/commands/status.md docs/commands/replace.md docs/commands/remove-missing.md`
   returns zero (the `--missing-id` references now read `devid`/`devids`);
   `rg "missing disk's name|device names and IDs" docs README.md` also returns
   nothing; `just docs-build` still passes linkcheck.
5. **VM warning test**: run the `tests/cli/braid-add-warnings.py` NixOS check
   (linux-builder, `aarch64-darwin`) to confirm the degraded-add warning renders
   the new Variant A wording end-to-end.

## Notes / out of scope

- Helper names (`see_*_in_status`) are a bikeshed; the implementer may rename to
  the module's taste as long as the family stays grouped and intent-doc'd.
- No behavior changes beyond wording and the `doctor.rs` plural/period fix; no
  new public CLI surface, no flag changes.
- `repair_hint`'s existing helpers (`missing_replace_command*`,
  `optional_missing_id_cross_check_phrase`, `hot_unplug_not_yet_missing`) are
  untouched -- this only adds the trailer family alongside them.
- Incidental: retire the one non-output "device IDs" phrase in a test
  doc-comment at `cli/src/alert.rs` (~:2002) to "devids", so the residual-drift
  gate (verification step 3) is a clean sweep rather than carrying a permanent
  carve-out. Comment-only; no behavior change.
- Left intentionally untouched (already-correct vocabulary, different surface):
  the `--missing-id` clap help at `cli/src/main.rs` (~:346) and the `--missing-id`
  field-description gloss in `docs/commands/{replace,remove-missing}.md` tables,
  which already say "btrfs devid".
