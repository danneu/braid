# Plan: document the `mountpoint_immutable` doctor check + guard against doctor docs drift

## Context

`braid doctor` runs an orchestrated set of diagnostic checks. One of them --
internal JSON name `mountpoint_immutable`, human label `mountpoint seal`,
implemented at `cli/src/doctor.rs#check_mountpoint_immutable` and wired into the
run order in `cli/src/doctor.rs#run_doctor` (between `braid_online_active` and
`wake_on_lan`) -- is **entirely absent** from `docs/commands/doctor.md`. It is
the only orchestrated check missing from the docs.

It emits a `mountpoint seal` row (Ok/Warn/Fail/Skip) on every run. An operator
who sees `[fail] mountpoint seal`, or a script keying on `mountpoint_immutable`
in `--json`, has no documented reference for what it means or how to remediate.

Root cause: commit `c4d21337 feat(seal-mountpoint): ...` added both the
`seal-mountpoint` command and this doctor check, but documented only the
command (`docs/commands/seal-mountpoint.md`). **Nothing enforces that every
doctor check appears in the docs table**, so it drifted silently -- the same
class of gap braid already guards elsewhere (`check-output-ascii.py`,
`check-see-paths.py`, `check-code-doc-anchors.py`, `mdbook-linkcheck2`, ...).

This plan does three things: (1) document the check; (2) correct a misleading
skip message the documentation surfaced (the standalone-install skip reads as
though the install is "module-managed" when `systemd_lifecycle` is in fact unset
-- standalone); and (3) add a parity guard so the next added check cannot drift
the same way -- matching braid's established "convert remember-to-do-X into
CI-fails-if-you-don't" pattern.

## Scope decision

**Docs + drift-guard** (user-selected). The docs fix alone patches this one
instance; the guard dissolves the recurrence class and is the in-character,
root-cause fix for braid. Review then surfaced a small adjacent correctness bug
(the misleading standalone-skip message); it is folded in as Part 2 because the
docs must describe the corrected behavior, not faithfully reproduce a wrong
message.

---

## Part 1 -- document the check in `docs/commands/doctor.md`

Four edits, all in `docs/commands/doctor.md`. Behavior wording is taken from
`cli/src/doctor.rs#classify_mountpoint_immutability` and
`cli/src/doctor.rs#check_mountpoint_immutable` (skip/warn/fail/ok messages).

### 1a. "What it checks" table row

Insert a row **between** the `braid_online_active` and `wake_on_lan` rows (table
order mirrors run order). Match the bold-`Warn`/`Fail`/`Skip` style of the
`foreign_luks_uuid` and `smart_self_test` rows:

```
| `mountpoint_immutable` | On `systemd_lifecycle` installs, verifies the offline-pool mountpoint seal -- the immutable attribute braid sets on the mount point while the pool is unmounted, so a stray write fails with `EPERM` instead of landing on the root filesystem and being hidden once the pool mounts (see [seal-mountpoint](seal-mountpoint.md)). **Warn** when the pool is offline but the mount point is mutable (it re-seals on the next boot or `nixos-rebuild switch`; run `braid seal-mountpoint` to re-seal now); **Fail** when the pool is mounted but the mount point inode is immutable (a live pool root must never be sealed -- a tripwire for a bug or external interference). **Ok** when the offline mount point is sealed, or when the pool is mounted (the live filesystem governs writes). **Skip** on standalone CLI installs (`systemd_lifecycle` unset, no NixOS module), where the boot-time seal this check verifies is not present, or when the mount-state / immutable-attribute probe is indeterminate. |
```

### 1b. Example-output block

Insert one line **between** the `braid-online` and `wake-on-lan` rows in the
"Basic example" output (run order). The example host has a mounted, healthy pool,
so the representative row is the online+mutable Ok message. Match the formatter's
spacing exactly: `cli/src/doctor.rs#format_doctor_human_with` renders each row as
`format!("{display_label:<14}  {}", message)` -- a minimum-14-wide left-aligned
label followed by **two literal spaces**. `mountpoint seal` is 15 chars, so it
overflows the 14-wide field (Rust's `:<14` is a minimum width, never truncates):
no padding is added and the two literal spaces sit directly after it. The result
is **two** spaces before the message -- it does not column-align with the shorter
labels, and that misalignment is the real formatter output:

```
[ok]   mountpoint seal  pool is mounted -- the live filesystem governs writes
```

### 1c. "What happens under the hood" step

Insert a new numbered step **between** the current UPS step (`braid-online.service`
check) and the current auto-suspend/`ethtool` step, then renumber the rest:

```
8. On `systemd_lifecycle` installs, probes whether the pool mount point is mounted and whether its inode carries the immutable attribute, then classifies the pair: offline+mutable -> Warn with a re-seal hint, mounted+immutable -> Fail, and the healthy pairs -> Ok. Standalone CLI installs (`systemd_lifecycle` unset, no NixOS module) skip it -- the boot seal exists only under the module. See [ADR 028](../design/decisions/028-immutable-unmounted-mountpoint.md).
```

### 1d. Reciprocal cross-link in "Related commands"

`docs/commands/seal-mountpoint.md` already links **to** doctor; doctor.md does
not link back. Add to doctor.md's "Related commands" list:

```
- [seal-mountpoint](seal-mountpoint.md) -- set or clear the offline mountpoint seal that the `mountpoint_immutable` check verifies
```

### Notes

- `docs/commands/doctor.md` frontmatter stays `experimental: false`. The check
  runs unconditionally and degrades to Skip; only the row text references the
  experimental `seal-mountpoint` feature. Do **not** flag the whole page
  experimental.
- No edit to the "Machine-readable output" section: it documents the generic
  `name`/`status`/`message`/`subject` envelope and does not enumerate check
  names. The "What it checks" table is the canonical name list. (The finding's
  mention of "JSON schema notes" as a gap is a minor overstatement -- there is
  no per-check JSON enumeration to update.)
- No README/index/guide change needed: `README.md` and `docs/index.md` list
  commands, not checks; `docs/guides/ups.md` documents only the UPS-specific
  checks. Confirmed via doc-surface sweep.

---

## Part 2 -- correct the misleading standalone-skip message (code + test)

Documenting the skip branch surfaced that its message is misleading.
`cli/src/doctor.rs#check_mountpoint_immutable` skips when
`!config.systemd_lifecycle()` -- the **standalone** case: `modules/braid/cli.nix`
emits `systemd_lifecycle = true`, while standalone configs omit it and it
defaults `false` (`cli/src/config.rs#parses_config_without_systemd_lifecycle_defaults_false`;
`systemd_lifecycle() == true` means "module-managed lifecycle enabled" per
`docs/design/decisions/018-systemd-lifecycle.md`). Yet the message reads
`skipped (systemd_lifecycle not configured -- the mountpoint seal is module-managed)`,
which a standalone operator can read as "this install is module-managed" (it is
not) rather than "the seal is a module feature this install lacks."

**Code fix:** reword the skip message in `cli/src/doctor.rs#check_mountpoint_immutable`
to name the standalone case and not imply the current install is module-managed,
keeping the `systemd_lifecycle not configured -- ` lead that the sibling
`braid_online_active` row already uses. Recommended (ASCII-only, per the
output-ascii rule):

```
skipped (systemd_lifecycle not configured -- standalone install; the offline mountpoint seal is a NixOS-module boot feature)
```

Behavior is unchanged -- standalone installs still skip (the warn/heal model
assumes the module's boot/activation re-seal, so there is no invariant to verify
without it); only the rationale text changes. Keep the 1a/1c docs wording in sync
with the final string.

**Focused test:** mirror the sibling regression test
`cli/src/doctor.rs#braid_online_check_skips_when_lifecycle_disabled` -- build a
`DoctorContext` from a standalone config (`systemd_lifecycle` omitted -> `false`)
and assert `check_mountpoint_immutable` returns `Skip` with the corrected
message. The skip returns before `online_ops` is touched, so the branch is
deterministic without root or a real mountpoint. Open with the standard
Intent / Why-it-exists / Scenario test preamble.

> Sibling scope (resolved -- do not expand): the sibling `braid_online_active`
> check shares the `!systemd_lifecycle()` gate but phrases its skip differently
> ("braid-online is not Rust-managed"). Review decided **not** to unify them: the
> sibling's wording is accurate (the install genuinely is not running the Rust
> `braid-online` lifecycle), so rewriting its pinned string in
> `braid_online_check_skips_when_lifecycle_disabled` would be churn, not a fix.
> The defect is specific to `mountpoint_immutable`'s "module-managed" phrasing,
> which a standalone operator misreads as "this install is module-managed". This
> plan corrects only `mountpoint_immutable`.

---

## Part 3 -- drift guard: `scripts/docs/check-doctor-table-parity.py`

New ~40-line script modeled on the existing `scripts/docs/check-*.py` guards
(same `ROOT = Path(__file__).resolve().parents[2]` resolution, same
`--selftest` convention, plain `re` + `sys.exit`).

**Invariant:** the complete set of doctor check names emitted by
`cli/src/doctor.rs#run_doctor` equals the set of names documented as rows in
doctor.md's "What it checks" table (bidirectional -- catches both undocumented
checks and stale doc rows).

**Code-side source of truth:** the `expected_names` inventory in
`cli/src/doctor.rs#valid_config_parses_ok_declared_disks_skips` -- a sorted
`vec!` of every distinct check name, asserted equal to the names actually
emitted by `run_doctor` (`assert_eq!(actual_names, expected_names)`), so it is
the complete check inventory by construction. Locate the
`let expected_names: Vec<&str> = vec![ ... ];` block and capture each `"..."`
literal. **Fail closed:** if the block cannot be located (e.g. the test was
renamed or restructured), exit non-zero with a clear message rather than
comparing against an empty set -- a silent pass would defeat the guard.

> Rejected alternative: the human-label match arms in
> `cli/src/doctor.rs#format_doctor_human_with`. They are production code, but the
> `other => other` catch-all renders a visible row for any check that lacks a
> quoted label arm -- so a future check added to `run_doctor` + `expected_names`
> but missing both its label arm and its doc row would be invisible to a
> label-arm-based guard and slip through, exactly the drift this guard exists to
> catch. `expected_names` has no such hole because the unit test pins it equal to
> the emitted check set.

**Docs-side source of truth:** rows under the `## What it checks` heading in
`docs/commands/doctor.md` (stop at the next `## ` heading). Capture the
first-column backtick token: `` r'^\|\s*`([a-z0-9_]+)`' ``.

**Behavior:** compare the two sets; on mismatch print the code-only names
(undocumented) and docs-only names (stale) and `sys.exit(1)`; else exit 0.

**`--selftest`:** run the parse + compare logic over in-memory fixtures (mirror
`scripts/docs/check-output-ascii.py`). The `expected_names` parser must accept
source text (not just a file path) so a fixture can exercise it. Cases: (a) a
matching code/doc pair passes; (b) an injected code-only name fails; (c) an
injected docs-only name fails; (d) **fail-closed** -- a fixture whose
`expected_names` block is absent or malformed makes the checker exit non-zero
with the clear "could not locate expected_names" message, never a silent
empty-set pass. (d) is the most important case: the whole guard rests on the
parser finding that block, so its failure mode must be self-tested. Proves the
checker logic -- including the fail-closed path -- before it runs against the
tree.

### Wiring

**Why the binding test runs in the same lane.** The guard's code-side source of
truth is `expected_names`, a test fixture. It is only proven equal to the live
`run_doctor` output when `cli/src/doctor.rs#valid_config_parses_ok_declared_disks_skips`
runs (`assert_eq!(actual_names, expected_names)`). If that binding test does not
run in the same lane as the parity check, a future check added to `run_doctor`
while *both* `expected_names` and the docs are forgotten leaves a stale-but-self-
consistent `expected_names`/docs pair, and the parity check passes on stale data.
So the binding test must run first, in the same lane, before the compare.

**CI reality (verified).** No always-on push/PR workflow runs the Rust tests:
`checks.yml` (always-on) is python-only on bare ubuntu; `test.yml` is
`workflow_dispatch`-only (VM-test triggers commented out to save Actions minutes);
`release.yml` runs `just test-rust` only on release tags. So the parity job cannot
lean on another lane to keep `expected_names` honest at PR time -- it must run the
binding test itself. The workspace root `Cargo.toml` has `default-members = ["cli"]`,
so `cargo test --lib <name>` from the repo root resolves to the cli crate.

- **justfile:** add a recipe alongside the other `check-*` recipes (near
  `check-doc-links` / `check-output-ascii`). Run the binding test first, then the
  selftest-then-real python guard:
  ```
  # Verify every braid doctor check has a row in docs/commands/doctor.md (and no
  # stale rows). The cargo test first pins expected_names == run_doctor output, so
  # the python guard's code-side source of truth cannot silently go stale.
  check-doctor-table:
      cargo test --lib valid_config_parses_ok_declared_disks_skips
      python3 scripts/docs/check-doctor-table-parity.py --selftest
      python3 scripts/docs/check-doctor-table-parity.py
  ```
- **`.github/workflows/checks.yml`:** add a new `doctor-table` job (each existing
  guard is its own job). Because the binding test must run in this same always-on
  lane, this is the one job that needs a Rust toolchain. Keep the inline-commands
  style (other jobs call `python3 ...` directly, not `just`):
  ```yaml
  doctor-table:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - name: Doctor table parity (bind expected_names, selftest, then parity)
        run: |
          cargo test --lib valid_config_parses_ok_declared_disks_skips
          python3 scripts/docs/check-doctor-table-parity.py --selftest
          python3 scripts/docs/check-doctor-table-parity.py
  ```

> Cost + alternatives (decide before implementing): this adds a Rust toolchain +
> crate compile to a lane that was seconds-fast python-only. Bounded (one crate;
> `rust-cache` warms incremental builds) and the price of a guard trustworthy in
> isolation. Rejected: (a) keep the parity job python-only and rely on the Rust
> lane -- no always-on lane runs the Rust tests, so nothing would gate
> `expected_names` freshness at PR time; (b) run full `just test-rust` as a new
> always-on job and keep parity python-only leaning on it -- heavier (lib + bins +
> 3 integration tests) and re-splits the binding test from the parity check across
> jobs, the cross-lane dependency this fix removes; (c) `nix develop --command`
> like `release.yml` -- heavier nix provisioning than `dtolnay/rust-toolchain` for
> one focused test. Adjacent, out of scope: that Rust unit tests run in no
> always-on PR lane at all is a broader CI-policy gap (VM-test triggers were
> disabled for cost); re-enabling a general Rust-test job is beyond this
> docs-parity plan.

---

## Files to modify

- `docs/commands/doctor.md` -- 4 edits (Part 1) + the corrected 1a/1c skip wording (Part 2).
- `cli/src/doctor.rs` -- reword the `check_mountpoint_immutable` standalone-skip message and add the focused skip-branch test (Part 2).
- `scripts/docs/check-doctor-table-parity.py` -- new guard (Part 3).
- `justfile` -- new `check-doctor-table` recipe (Part 3).
- `.github/workflows/checks.yml` -- new `doctor-table` job, with a Rust toolchain so the binding test runs in the same always-on lane as the parity check (Part 3).

## Verification

1. **Guard catches the gap (TDD):** write the script, then run
   `python3 scripts/docs/check-doctor-table-parity.py` **before** the doctor.md
   edits -- it must fail naming `mountpoint_immutable` as code-only (undocumented).
   This proves the guard works and reproduces the finding.
2. **Self-test:** `python3 scripts/docs/check-doctor-table-parity.py --selftest`
   passes (fixtures detect injected mismatches).
3. **Parity after docs edit:** apply Part 1, re-run the bare guard -- it now
   passes (all 18 check names accounted for, no stale rows).
4. **Recipe + CI, including the binding:** `just check-doctor-table` passes
   end-to-end (cargo binding test -> python selftest -> python parity). Then prove
   the binding actually gates drift: add a throwaway `checks.push(...)` for a fake
   check name to `run_doctor` *without* touching `expected_names`, and confirm
   `just check-doctor-table` fails at the `cargo test` step (stale `expected_names`)
   rather than silently passing the python compare. Revert the throwaway.
5. **Existing doc guards still green:** `just check-docs` (and the other
   `scripts/docs/check-*.py` recipes) -- ensure the new table row and the
   `seal-mountpoint` link don't trip `check-doc-tables.py` / `check-doc-links.py`
   / link validation.
6. **Render check:** `just docs-build` succeeds (mdbook-linkcheck2 validates the
   new internal links to `seal-mountpoint.md` and ADR 028).
7. **Spot-check wording:** the row's Warn/Fail/Skip text matches the literal
   messages in `cli/src/doctor.rs#classify_mountpoint_immutability` and
   `cli/src/doctor.rs#check_mountpoint_immutable` (including the corrected skip
   string).
8. **Skip-message fix (TDD):** add the focused test first and confirm it fails
   against the current "module-managed" string, then reword the message so it
   passes. `just test-rust` (or `cargo test`) -- the new test asserts a standalone
   config yields `mountpoint_immutable` = Skip with the corrected message, and the
   sibling `braid_online_check_skips_when_lifecycle_disabled` still passes
   unchanged.
9. **ASCII gate:** `just check-output-ascii` stays green (the reworded skip
   message is ASCII-only).

## Out of scope

- Unifying the check registry (one table that run-order, display labels, and
  docs all derive from) -- a real refactor of working code that this finding does
  not justify. The guard achieves code<->docs parity without that churn.
- Documenting per-check names in the "Machine-readable output" section.

## Implementation notes

- Part 3 selftest: the plan's fail-closed case (d) ("`expected_names` block
  absent or malformed") is implemented as two distinct fixtures -- (d) an absent
  block (asserts the "could not locate" message) and (d') a present-but-empty
  `vec![]` (asserts the no-literals branch). Both exits are non-zero; splitting
  them pins each fail-closed branch of `parse_expected_names` independently.
