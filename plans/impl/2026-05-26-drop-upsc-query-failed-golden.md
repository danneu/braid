# Drop the vacuous `golden_upsc_query_failed` golden test + its orphaned fixture

## Context

`golden_upsc_query_failed` (`cli/tests/support/golden_common.rs:600-625`) claims
to "lock in the non-zero-exit contract" for `braid ups status` using the
captured `upsc-daemon-down.stderr` fixture. It does not, and cannot:

- **Routing ignores stderr.** `query_ups` (`cli/src/ups.rs:54-65`) decides
  `QueryFailed` vs `Ok` purely on `raw.exit_status != 0` (line 58). stderr is
  only a payload field.
- **The exit code is a test literal, not the fixture.** The test hardcodes
  `exit_status: 1` in the `MockRunner` output. The fixture is a stderr-only
  file (`Error: Connection failure: Connection refused`); the capture script
  (`tests/capture-ups-fixtures.py:45-49`) explicitly discards upsc's exit code
  ("we want the stderr regardless of exit code").
- **The one fixture-backed assertion is a tautology.** The test feeds
  `stderr.clone()` into the mock, then asserts `got.contains(stderr.trim())`.
  Since `query_ups` just trims and returns that same stderr, this can never
  fail. The fixture contributes zero test signal.
- **It is redundant and mislaid.** The unit test
  `query_ups_returns_query_failed_on_non_zero_exit` (`cli/src/ups.rs:594-608`)
  already proves exit-1 -> `QueryFailed` with the exact same stderr text, and
  the VM test `tests/cli/braid-status-ups.py:79-101` proves the *real*
  end-to-end non-zero-exit contract against a genuinely stopped `upsd`. A
  query-failed case parses nothing (empty stdout), so per AGENTS.md and the
  `ups.rs:588-589` docstring ("non-zero exit is runner-integration state, not
  parser state") it does not belong in the parser/golden lane at all.

**Intended outcome:** remove the vacuous test and everything it orphans, leaving
the non-zero-exit contract covered where it belongs (the unit test for the
routing boundary; the VM test for the real exit code). We deliberately do **not**
take the finding's alternative of capturing the exit code into the fixture --
that would deepen the parser lane's investment in runner-integration state it
explicitly disclaims, duplicating the VM test.

## Change

Five edits: three deletions (test + two fixtures), one block-removal-plus-comment
edit, and one comment-only count fix. Blast radius verified clean.

1. **Delete the test + its preamble** -- `cli/tests/support/golden_common.rs`,
   lines `593-625` (the `// Intent/Why/Scenario` block through the closing `}`
   of `fn golden_upsc_query_failed`). No import cleanup needed: `RawCommandOutput`
   stays used by lines 49 and 471, and `MockRunner` / `CmdRequest` / `query_ups`
   / `UpsQueryError` are all inline-qualified (`braid_cli::...`) inside the
   deleted test, so there are no `use` lines to touch.

2. **Delete the stable fixture** --
   `cli/tests/fixtures/nixos-25.11/upsc/upsc-daemon-down.stderr`.

3. **Delete the unstable mirror** --
   `cli/tests/fixtures/nixos-unstable/upsc/upsc-daemon-down.stderr`.

4. **`tests/capture-ups-fixtures.py` -- remove the capture block + fix the header:**
   - Delete the `# --- Daemon-down stderr ---` block (lines `44-50`): the
     `systemctl stop upsd.service` line and the `machine.execute(... 2>
     .../upsc-daemon-down.stderr)` call. Nothing after it depends on upsd's
     state -- the only remaining step is the wildcard `machine.copy_from_vm`
     (lines 52-54), which still copies the four live-state fixtures.
   - In the header docstring (lines 1-8): drop the "A fifth capture -- daemon
     down" sentence, AND correct the pre-existing miscount "five dummy-ups
     drivers, one per target state" -> "four" (the states it then lists are
     already only four: online/onbattery/lowbattery/replace-battery).

5. **`tests/capture-ups-fixtures.nix` -- comment-only count fix (pre-existing
   drift):** The `states` map has four entries and the drivers + `.dev` files are
   generated 1:1 from it (lines 139, 146), yet the header still says "five
   `dummy-ups` drivers" (line 4) and "why 5 .dev files" (line 16). This miscount
   predates and is independent of the daemon-down removal -- daemon-down was never
   a driver or a `.dev` file. But since change #4 already corrects the same count
   in the `.py` header, and that header points readers here ("See the .nix header
   for context"), fix both occurrences to "four" so the pair stays consistent.
   Comment-only; no behavior change.

## What we deliberately do NOT change

- **Historical plan docs** under `plans/impl/` (`2026-05-04-split-upsc-query-parsing.md`,
  `2026-05-18-collapse-legacy-upsc-fixtures.md`, etc.) reference the test and
  fixture. These are point-in-time implementation records; editing them to scrub
  references would falsify the archival history. Leave them untouched.
- **`tests/capture-ups-fixtures.nix` capture logic** -- the `states` map and the
  driver / `.dev` generation are unchanged; only the stale header *comment* count
  is corrected (change #5). No state is added or removed.
- **`justfile` / `flake.nix`** -- the capture recipes use a wildcard copy of
  `result/fixtures/*`; they work unchanged with four fixtures.
- **Fixture-dir READMEs** -- they do not enumerate individual fixtures.
- **`query_ups_returns_query_failed_on_non_zero_exit`** (`cli/src/ups.rs:594`)
  and **`tests/cli/braid-status-ups.py`** -- these are the keepers that carry the
  contract; do not touch them.

## Verification

Run all four. The change touches the shared golden harness (which compiles into
**both** fixture lanes) and the capture VM-test script, so stable-only
verification is insufficient.

1. `just test-rust` -- stable lane (`cargo test ... --test golden_nixos_25_11`,
   justfile:108-109). Confirms (a) the crate + shared `golden_common.rs` compile,
   so no orphaned imports; (b) the four remaining upsc golden tests
   (`golden_upsc_online`/`_onbattery`/`_lowbattery`/`_replace_battery`) still
   pass; (c) `query_ups_returns_query_failed_on_non_zero_exit` (`cli/src/ups.rs:594`)
   -- now the sole unit-level owner of the routing contract -- still passes.
   Note: this lane skips-when-missing (`REQUIRE_FIXTURES = false`,
   `golden_nixos_25_11.rs:10`), so it cannot by itself prove the deleted fixture
   reference is gone -- step 4 is that proof, and step 2 (fail-on-missing) is the
   backstop.
2. `just test-rust-unstable` -- unstable lane (`cargo test --test
   golden_nixos_unstable`, justfile:186-187). This is the lane `just test-rust`
   does **not** cover. It `include!`s the same edited `golden_common.rs` but with
   `REQUIRE_FIXTURES = true` (fail-on-missing, `golden_nixos_unstable.rs:11`). It
   passes against the four remaining committed `nixos-unstable/upsc/*` fixtures;
   no re-capture is needed, because deleting `golden_upsc_query_failed` removes
   the only reference to the deleted fixture.
3. `nix build .#checks.<system>.capture-ups-fixtures -L` (e.g. `aarch64-darwin`)
   -- exercises the edited capture VM-test script end-to-end, so a broken edit to
   `capture-ups-fixtures.py` (or the `.nix` comment edit) fails here. Use the bare
   `nix build` of the check, **not** the full `just capture-ups-fixtures` recipe:
   the recipe additionally runs `cp -f result/fixtures/* .../upsc/`
   (justfile:142-144), which would overwrite the committed fixtures. We are
   validating the script, not refreshing fixtures (no tool-version bump), so the
   copy step is unwanted here.
4. `rg -n "upsc-daemon-down|golden_upsc_query_failed" cli/ tests/` -- expect
   **zero** hits. Historical references survive only under `plans/impl/`, which is
   outside this search and intentionally left as archival record.

Optional, unchanged code: `just test-vm braid-status-ups` re-confirms the real
end-to-end non-zero-exit contract (`tests/cli/braid-status-ups.py:79-101`) -- the
keeper that justifies dropping the golden test. Not required, since that test is
untouched.
