# Refactor: kill line-number citations of tracked files + guard against recurrence

## Context

`docs/dev/doc-citations.md#doc-and-adr-file-references` is law: a durable braid
artifact must never cite another tracked braid file by line number. Use
`path#symbol` (code) or `path#heading-slug` (markdown) instead. Line numbers drift
the instant surrounding content moves, so the pointer silently goes stale and
misleads the next reader. The rule applies to "docs and comments"; only
`plans/wip/` transient analysis is exempt.

A `/verify-issue` pass on a Low/Testing finding (stale `~line 243`/`~line 276` in
a VM-test preamble) confirmed the finding. braid's house style is to encode
conventions as executable guards (9 `check-*.py` scripts run in CI), so a fix
without a guard is incomplete -- the finding's own thesis is "line numbers rot,"
and the sweep proves it: violations exist in every form (`~line`, `near line`,
`.rs:NN`, `.md:NN`, `.nix (lines N-M)`, `ADR NNN:NN`) across code, tests, and
active docs.

### Scope note -- REVISED AFTER REVIEW (needs sign-off)

An earlier draft chose a *narrow* guard (source files only; scan `cli`/`tests`/
`modules`, not `docs`). That choice rested on an inventory that **missed most of
the violations** -- it claimed one live `docs/` instance when there are several,
including an active ADR-to-ADR citation. A narrow guard would leave 4+ active
violations unfixed and unguarded (CI green while the repo violates the rule).
This revision **pivots to a broad guard** that enforces the actual rule:
*no durable tracked-file line citation*, scanning `cli`/`tests`/`modules`/`docs`,
with convention-defined exemptions (frozen ADRs, the rule's own doc, `reference/`,
`plans/`). This reverses the earlier "scoped lint" decision; flagged for the
operator's approval. (Reviewer rounds 2-3 endorse the broad direction -- F4
"no broader pivot," Viability "the broad guard pivot is the right shape" -- so the
sole open question is the operator signing off on reversing their own earlier
narrow choice.)

## Verified inventory (corrected, exhaustive sweep)

**Fix (10 live violations -- durable artifact cites a tracked file by line):**

| # | Location | Citation | Form |
|---|----------|----------|------|
| 1 | `tests/cli/remove-missing-state-dir-readonly.py:24` | `remove_missing.rs ~line 243` (stale) | `~line` |
| 2 | `tests/cli/remove-missing-state-dir-readonly.py:25` | `save_membership (~line 276)` (stale) | `~line` |
| 3 | `tests/cli/replace-preformatted-luks-passphrase-mismatch.py:19` | `near line 214` (stale) | `near line` |
| 4 | `cli/src/tui/probe.rs:538` | `modules/braid/fan-control.nix` (lines 166-187) | `(lines N-M)` |
| 5 | `cli/src/alert.rs:135` | `014-alerts.md:74` | `.md:NN` |
| 6 | `cli/src/monitor.rs:733` | `ADR 014:74` | `ADR NNN:NN` |
| 7 | `tests/repro/btrfs-replace-rejected-during-scrub.py:19` | `testing.md:64-72` | `.md:NN-MM` |
| 8 | `docs/internals/real-world/sata-hot-unplug.md:153` | `probe_pool()` ... (lines 190-206) | `(lines N-M)` |
| 9 | `docs/internals/real-world/sata-hot-unplug.md:156` | MISSING filtering (line 116) | `(line N)` |
| 10 | `docs/design/decisions/019-inhibit-sleep.md:156` (status: Active) | `018-systemd-lifecycle.md:131` | `.md:NN` |

**Leave (exempt -- guard must skip):**

- `docs/design/decisions/021-wait-in-unlock.md:100` (`unlock.rs:93-96`) -- **Superseded**; convention forbids repointing frozen docs.
- `docs/dev/doc-citations.md:18` (`cli/src/cmd/unlock.rs:142`) -- the rule's own **negative example**.
- 25+ `reference/...` and bare upstream `.c:NN` cites -- governed by `reference-source.md`.
- `plans/**` (incl. promoted `plans/impl/2026-*.md`, which carry many cites) and `prompts/**` (e.g. `command-review-fanout.md:75`'s `[#3](./add.md:147)` is an *example* linking into transient `command-findings/`) -- plan analysis / agent tooling, not durable reference prose. See Part 4.

**Clean separately (Part 2) -- dangling refs into a deleted plan:**
`cli/src/journal.rs:78` (`plan lines 979-987`), `:291-292` (`plan lines 1120-1140`), `:1252` (`plan line 446`), `cli/src/discover.rs:1748` (`plan lines 4123-4129`).

## Part 1 -- Fix the 10 live citations

Each edit drops the line number and anchors by an already-greppable symbol or,
for heading targets, by the slug of the section that documents *what the citing
comment means* -- verified against that section's content, not by whichever
heading happens to contain the (now-stale) cited line. This distinction is
load-bearing: for #5/#6, #7, and #10 the cited line already sat in the *wrong*
section, so a mechanical "heading spanning the cited line" substitution would
re-encode the very staleness the rule forbids. Keep test preambles faithful to
the mandated `Intent / Why it exists / Scenario` structure (`docs/dev/testing.md`).
Comment/prose-only.

### Code & test comments

- **#1-2 `remove-missing-state-dir-readonly.py`** -- verified order in
  `remove_missing.rs#RemoveMissingPlan::execute`: `journal::write_journal` (216)
  -> `pool_remove_device_using` (229) -> `membership::save_membership` (249).
  Rewrite the clause to: *"`journal::write_journal` precedes the btrfs mutation
  (`pool_remove_device_using`), and `save_membership` follows it and propagates
  errors via `?`."* Keep `commit a9b7467` (a hash is a durable anchor).
- **#3 `replace-preformatted-luks-passphrase-mismatch.py`** -- the cited
  `PresentLuks { mapper_open: false }` conflated the config-state enum
  (`types.rs#PresentConfigDiskState`) with the actual reversible, pre-journal
  check `replace.rs#verify_existing_luks_new_target_preflight` (dispatches the
  closed-LUKS case on `ReplaceTargetPrep::ExistingLuks { mapper_open: false }`).
  Anchor on `verify_existing_luks_new_target_preflight`, contrasting the
  fresh-format (`PresentNotLuks`) branch.
- **#4 `tui/probe.rs:538`** -- cites the pwm-resolution `script` block of the
  fan-control systemd unit. Nix has no greppable symbol there; drop
  `(lines 166-187)` and use the whole-file referent (`modules/braid/fan-control.nix`)
  or name the stable `fc.pwm.platformDevice`/`fc.pwm.number` options it mirrors.
- **#5 `alert.rs:135` / #6 `monitor.rs:733`** -- both comments are about the
  *fail-closed `ComputationError`* contract: a corrupt `acked-stats.json` must
  surface as `ComputationError` (exit 1), never be silently treated as an empty
  baseline (fail-open). That contract is documented in the "braid monitor is a
  pure detector" section ("Fail closed: any failure inside cmd_monitor that
  leaves pool state indeterminate latches a ComputationError cause"), **not** in
  the section containing line 74 ("Ack state keyed by btrfs devid", about devid
  membership cross-reference -- the cite already pointed at the wrong section).
  Anchor both on the meaning:
  - `alert.rs:135` -> `docs/design/decisions/014-alerts.md#braid-monitor-is-a-pure-detector`.
  - `monitor.rs:733` -> rewrite the `ADR 014:74` shorthand to the same full
    linkable form, e.g. *"the fail-closed detector contract
    (`docs/design/decisions/014-alerts.md#braid-monitor-is-a-pure-detector`)."*
  Both then fall under `check-code-doc-anchors.py`, which validates the anchor
  resolves (a wrong slug fails CI).
- **#7 `btrfs-replace-rejected-during-scrub.py:19`** -- the preamble calls this
  test a "live-tool behavior-lock" and cites testing.md for the
  `cryptsetup-close-mounted.py` example of that pattern. Both live in the
  "Live-tool behavior locks" section (which names `cryptsetup-close-mounted.py`
  explicitly), not lines 64-72 (f-strings gotcha / "Regression test quality" --
  the cited span was already wrong). Replace `docs/dev/testing.md:64-72` with
  `docs/dev/testing.md#live-tool-behavior-locks`.

### Docs prose

- **#8 `sata-hot-unplug.md:153`** -- drop `(lines 190-206)`; `probe_pool()` is
  already named. Keep the `` `path` -- prose `` list style.
- **#9 `sata-hot-unplug.md:156`** -- drop `(line 116)`; anchor on
  `parse_devid_line` (the present/missing split lives in the loop driven by it).
- **#10 `019-inhibit-sleep.md:156`** -- the cite is about the *Rust dispatch
  post-lock `mark_offline`, gated on `systemd_lifecycle`* -- documented in the
  "Rust dispatch as synchronization layer" section ("Synchronization lives in
  Rust dispatch ... owns the pool lock, braid-online.service lifecycle updates").
  Line 131 is in the "braid-alert.service + braid-beep.service -- notification"
  section (the cite already pointed at the wrong section). Replace
  `018-systemd-lifecycle.md:131` with
  `docs/design/decisions/018-systemd-lifecycle.md#rust-dispatch-as-synchronization-layer`;
  018 is Active, so a forward heading link is correct, and the `.md`->`.md` link
  is validated by `mdbook-linkcheck2`.

## Part 2 -- Clean dangling plan-line refs

Drop the parenthetical; surrounding prose already carries the reasoning.

- `journal.rs:78` -- drop `(see plan lines 979-987)`.
- `journal.rs:291-292` -- drop `(see plan lines 1120-1140)`.
- `journal.rs:1252` -- replace `This pins plan line 446's fourth case.` with a
  self-contained sentence (e.g. `Pins the unknown-top-level-key case of the
  journal's deny_unknown_fields contract.`).
- `discover.rs:1748` -- replace `... pin from plan lines 4123-4129.` with
  `Why: pins the cloned-disk (duplicate-UUID) friendly-error behavior.`

## Part 3 -- Recurrence guard: `scripts/docs/check-line-cites.py`

Pivoted from "source-file line cites" to **durable tracked-file line cites**, to
match the actual rule. Mirror `scripts/docs/check-plans-refs.py` (structural
template): module docstring, `iter_files()` with standard `skip_dirs` + the
`docs/book` skip, `--selftest` over tempdir fixtures, failures to stderr under
`line cites check failed:`, success to stdout, exit 1/0.

The closest *sibling* is `scripts/docs/check-code-doc-anchors.py`: it already
scans `cli`/`tests`/`modules` (+ `prompts`, AGENTS/README) for `docs/*.md#anchor`
citations and validates each anchor resolves (stdlib + the local `_mdslug`
slugifier, selftest over tempdir fixtures). The new guard is **complementary,
not redundant**: that one validates anchor-style cites resolve; this one
*forbids* line-number cites. Its `#anchor`-required regex never matches a colon
line cite (`014-alerts.md:74`) or the `ADR NNN:NN` shorthand -- exactly the gap
this guard closes. Together they enforce "cite by anchor, and make the anchor
resolve."

**Scan roots:** `cli`, `tests`, `modules`, `docs` (book excluded). `plans/` and
`prompts/` are *not* scanned (Part 4).

**Flag (a tracked file cited by line):**

1. `(?P<path>[\w./-]+\.(?:rs|py|nix|md)):\d+(?:-\d+)?` -- `path:line`/`path:N-M`
   (now incl. `.md`). **Skip if `path` contains `reference/`.** (`.c`/`.h`
   upstream cites fall out naturally -- extension not in the set.)
2. `\bADR\s+\d+:\d+\b` -- the `ADR NNN:NN` shorthand (#6).
3. A parenthesized `\(lines?\s+\d+(?:-\d+)?\)` **on a line that also contains a
   tracked `\.(rs|py|nix|md)` path not under `reference/`** -- catches #4, #8, #9
   without tripping on upstream `balance.c:558-561` (colon form, no parens) or
   generic "(line N)" prose.
4. `~\s*line\s+\d+` and `\bnear\s+line\s+\d+` -- the original idioms (#1-3).

**Exemptions:**

- Path tokens containing `reference/` (upstream, per `reference-source.md`).
- The file `docs/dev/doc-citations.md` -- it ships negative examples; skip by name.
- Decision docs with frontmatter `status:` of `Superseded`/`Deprecated` -- skip
  (handles ADR 021). Extract the status with a **stdlib-only** parser: `re.match`
  the leading `---\n...\n---\n` block (as `check-frontmatter.py` does for the
  block boundary) and scan it for `^status:\s*(\w+)`. Do **not** `import yaml`.
  Rationale: the `checks.yml` jobs run bare `python3 scripts/docs/...` with no
  dependency-install step (e.g. the `plans-refs` job, `checks.yml:51`), and every
  script in that lane is stdlib-only. `check-frontmatter.py` is the *lone*
  `import yaml` user and is wired only through the Nix devshell (`justfile:320`),
  never in `checks.yml`; reusing its yaml-based shape would make the new
  bare-`python3` job fail at import on a clean runner before it lints anything.

**Failure message:** ``{rel}:{line}: line-number citation of a tracked file
`{match}`; cite `path#symbol` or `path#heading-slug` (see docs/dev/doc-citations.md)``.

**Selftest fixtures** -- bad: `~line 243`, `near line 214`, `foo.rs:142`,
`bar.md:64-72`, `ADR 014:74`, `` `x.nix` (lines 1-9) ``. ok: `journal::write_journal`
(symbol), `reference/btrfs-progs/cmds/balance.c:558` and bare `balance.c:558-561`
(upstream), `plan line 446` (not this rule's job), a `status: Superseded` fixture
with a cite (skipped), a fixture named `doc-citations.md` with a cite (skipped).

**Wiring (mirror plans-refs):**
- `justfile`: add `check-line-cites` recipe after `check-plans-refs` (`justfile:332`)
  -- two lines (`--selftest`, then live scan).
- `.github/workflows/checks.yml`: add a `line-cites` job mirroring the
  `plans-refs` job (`checks.yml:51`).

## Part 4 -- Document the plans/ and prompts/ exemptions (flagged)

The sweep surfaced many line cites in `plans/impl/*.md` and example links in
`prompts/*.md`. `doc-citations.md` only names `plans/wip/` as exempt, leaving
`plans/impl/` (promoted, dated, archived analysis) ambiguous; `check-plans-refs.py`
already declines to scan `plans/` at all. Recommend a one-line clarification in
`docs/dev/doc-citations.md` that the plan-analysis exemption covers all of
`plans/` (working `wip/` and archived `impl/` alike, as point-in-time records),
and note that `prompts/` are agent tooling. This aligns documented policy with the
guard's scope so the exemption is defensible by the doc, not by reviewer judgment.
*Operator: confirm -- alternative is to treat `plans/impl/` as in-scope and fix
its cites, which rewrites dated historical records (not recommended).*

## Out of scope (with rationale)

- **Frozen ADR `021-wait-in-unlock.md`** -- convention mandates not repointing
  Superseded/Deprecated docs; the `> Superseded by ...` banner is the pointer.
- **`reference/` upstream cites** -- governed by `reference-source.md` (cite by
  shape, not `path#symbol`); pinned shallow clones, line numbers more stable.
- **Linting plan-line refs (Part 2 forms)** -- ephemeral-artifact cites are
  `check-plans-refs.py`'s conceptual domain and the pattern is fuzzy; cleaned once
  here, not policed by this guard.

## Critical files

- Edit (fixes): `tests/cli/remove-missing-state-dir-readonly.py`,
  `tests/cli/replace-preformatted-luks-passphrase-mismatch.py`,
  `tests/repro/btrfs-replace-rejected-during-scrub.py`, `cli/src/tui/probe.rs`,
  `cli/src/alert.rs`, `cli/src/monitor.rs`, `cli/src/journal.rs`,
  `cli/src/discover.rs`, `docs/internals/real-world/sata-hot-unplug.md`,
  `docs/design/decisions/019-inhibit-sleep.md`; (Part 4) `docs/dev/doc-citations.md`.
- Add: `scripts/docs/check-line-cites.py`.
- Wire: `justfile`, `.github/workflows/checks.yml`.
- Read-only refs: `scripts/docs/check-plans-refs.py` (structural template),
  `scripts/docs/check-code-doc-anchors.py` (sibling guard; the `_mdslug` slug
  convention the replacement anchors must match), `scripts/docs/check-frontmatter.py`
  (frontmatter block shape -- but do **not** inherit its `yaml` import; see Part 3),
  the cited code/docs files above, `docs/dev/doc-citations.md`, `docs/dev/testing.md`.

## Verification

1. **Guard catches every bug first (TDD):** add `check-line-cites.py`, run it
   against the *unfixed* tree -- it must flag all 10 violations and **none** of
   the exempt cites (021, doc-citations.md:18, `reference/`, `plans/`, `prompts/`).
2. **Selftest:** `python3 scripts/docs/check-line-cites.py --selftest` -> ok.
3. **Apply Parts 1-2 (+4), re-scan:** `just check-line-cites` -> passes, proving
   all 10 live cites are gone and exemptions hold.
4. **No behavior change:** `cargo build` + `cargo test --lib` (Rust edits are
   `//`/`///` comments only -- no code fences, no doctest impact). VM-test edits
   are preamble-only; do not re-run the heavy NixOS lane.
5. **Docs + anchor cites intact:** `just check-docs` + mdbook build -- edits only
   swap line numbers for heading slugs; confirm no `mdbook-linkcheck2` regressions
   (heading-slug links ARE validated, unlike the removed line numbers). The new
   `docs/...md#slug` anchors in `alert.rs`/`monitor.rs` are *additionally*
   validated by `check-code-doc-anchors.py` (run via `just`): a wrong slug fails
   CI -- precisely the durability the rule buys.
6. **Full guard suite green:** run all existing `check-*` recipes plus the new one.
