# Drop legacy disk-name-keyed pool.json support

## Context

`pool.json` used to be keyed by disk name (e.g. `"toshiba": {"by_id": ...}`).
Decision 024 made the LUKS UUID the canonical persistent identity, so
`pool.json` is now keyed by LUKS UUID with the disk name demoted to a
value-side field. The one production system has been migrated to the new
shape, so the migration runbook, the legacy-shape sniff, the migration hint
in `discover`, and the cutover integration test are now dead weight.

Goal: remove all code, tests, and docs that exist to detect, hint at, refuse,
or migrate the legacy name-keyed `pool.json`. A legacy file, if one ever
appears again, will simply look corrupt to the loader and route through the
existing rebuild-from-live-LUKS-labels path (`discover --write` writes a
forensic sidecar, then rebuilds). That path was already there as the corrupt
rebuild flow; legacy files just collapse into it.

Out of scope (kept as-is):
- The `LuksUuidMap` deserializer that rejects non-UUID keys -- this is the
  current invariant, not legacy support.
- The `MembershipError::Corrupt` remediation suffix pointing at
  `braid discover --write` -- it handles any corrupt `pool.json`, not just
  the legacy shape.
- The `strip_legacy_managed_format_opts` test helper and fixtures journaling
  `--label`/`--uuid` extras -- adjacent pre-cutover artifact, not the same
  system.
- `scripts/braid-destroy.sh` UUID-keyed regex check -- it validates the
  current invariant.

## Behavior changes (operator-visible)

After this change, given a legacy name-keyed `pool.json`:

| Command | Before | After |
| --- | --- | --- |
| `braid discover` (preview) | Prints migration hint, continues preview | Prints "corrupt or unreadable -- run `braid discover --write` to rebuild" and exits 1 |
| `braid discover --write` | Refuses with `NameKeyedPoolJson` error | Writes forensic `pool.json.corrupt-<RFC3339-UTC>` sidecar, then rebuilds from live LUKS labels |
| `braid unlock` / any loader | Already fails with `MembershipError::Corrupt` | Unchanged |

This is strictly cleaner: a legacy file is just a corrupt file. No
special-casing.

## Files to edit

### `cli/src/discover.rs`

- Delete `DiscoverWriteError::NameKeyedPoolJson` variant (lines 176-182).
- Delete `PoolJsonShape::LegacyNameKeyed` variant (line 240) and update the
  enum's doc comment to drop the legacy reference (lines 233-237).
- In `classify_pool_json` (lines 248-264): collapse the `Err(_)` arm to
  return `PoolJsonShape::Corrupt` directly; drop the `is_legacy_name_keyed_shape`
  call.
- Delete `fn is_legacy_name_keyed_shape` (lines 266-282) entirely.
- In `write_discovered_membership` (lines 574-614): delete the
  `PoolJsonShape::LegacyNameKeyed => Err(NameKeyedPoolJson { ... })`
  arm (lines 611-614). A legacy file now reaches the `Corrupt` arm,
  which is correct.
- Rewrite the entire `write_discovered_membership` function-level doc
  comment (lines 574-596). Today it says "The four gates pinned in the
  plan must fire BEFORE any `save_membership` call" with gate 2 being
  the legacy name-keyed refusal; that gate is gone. The rewritten
  comment must:
  - Say "three gates" and renumber: (1) `pending-op.json` must not
    exist, (2) existing `pool.json` must not be a healthy UUID-keyed
    membership (`Corrupt` is the intentional rebuild path per
    decision 017), (3) the forensic sidecar must succeed before
    overwriting a corrupt `pool.json`.
  - Drop "cutover partial-attach" from the `expected_count` paragraph
    and reframe it as a generic guard: refuses if the produced
    membership count is not exactly `expected_count`, catching a
    momentarily detached disk or stray braid-labeled disk during any
    `discover --write` rebuild.
- Delete the `discover_write_refuses_when_pool_json_is_name_keyed` test
  (lines 1826-1852).
- Delete the `classify_pool_json_returns_legacy_name_keyed_for_name_keyed_shape`
  test (lines 2058-2074).
- Add a small test that pins the new behavior: a name-keyed pool.json now
  classifies as `Corrupt`, and `discover --write` rebuilds it via the
  forensic-sidecar path (one test, parallel to the existing corrupt-rebuild
  test).

### `cli/src/main.rs`

- In the `Commands::Discover` branch (lines 773-812): collapse the
  `Missing | LegacyNameKeyed` arm to just `Missing`, and delete the
  `eprintln!` legacy hint (lines 783-796). Update the comment block at
  lines 774-780 to drop the `LegacyNameKeyed` mention.

### `cli/src/membership.rs`

- Rename `load_membership_rejects_old_name_keyed_format` (lines 1022-1045)
  to `load_membership_rejects_non_uuid_top_level_keys` and drop the "old"
  framing in the intent comment. The behavior tested -- non-UUID keys in
  `disks` fail to load -- is the current `LuksUuidMap` invariant, not
  legacy-specific.
- `load_membership_rejects_hybrid_uuid_and_name_keys` (lines 1082-1096):
  keep as-is. It pins partial-load rejection, which is still load-bearing.

### `cli/src/journal.rs`

- Rename `old_name_keyed_targets_fails_parse_with_remediation_phrase`
  (lines 794-840) to `non_uuid_keyed_targets_fails_parse_with_remediation_phrase`
  and update the doc comment (lines 798-800) to drop the legacy framing.
  Still pins the journal `Parse` error wording, which is current behavior.

### `docs/decisions/024-luks-uuid-identity.md`

- Line 74: "The old model had a name-keyed map plus value-side fields..."
  -- delete the contrast clause. Replace with a flat statement of the new
  shape (no historical comparison).
- Line 165: drop "reject old name-keyed maps" from the test summary.
- Lines 218-220: delete the "Old name-keyed `pool.json` and old journal
  shapes are rejected rather than migrated. Braid is unreleased, so
  operators cut over..." consequence. This entire bullet is about the
  removed migration concern.

### `docs/luks-unlock.md`

- Delete the "legacy name-keyed" subsection (lines 166-170) entirely.

### `manual/commands/discover.md`

- Line 13: drop "or to migrate the legacy name-keyed shape -- see the
  runbook in `docs/luks-unlock.md`".
- Line 56: drop the sentence about legacy name-keyed `pool.json` refusal
  and migration hint; keep the rest of the step (healthy-UUID refusal +
  corrupt rebuild flow).
- Line 70: drop "Legacy name-keyed files are allowed only for read-only
  preview".

### `manual/guides/troubleshooting.md`

- Lines 37-42: rewrite the note. Do not preserve "remove it first" /
  `sudo rm /var/lib/braid/pool.json` as blanket advice -- it bypasses the
  forensic-sidecar invariant for corrupt/unreadable files (the rebuild
  path preserves the original bytes at `pool.json.corrupt-<RFC3339-UTC>`,
  which may carry prior-binding data like a `devid` for a
  `null_underlying` member). The rewrite splits the two cases:
  - **Corrupt / unreadable `pool.json`** -- run `braid discover --write`
    directly. The sidecar is written automatically; do not `rm` first.
  - **Healthy UUID-keyed `pool.json`** -- `discover --write` refuses on
    purpose; the normal path is `braid add` / `remove` / `replace`. If
    you have deliberately decided to re-discover instead, `mv` the file
    aside (do not `rm`) before running `braid discover --write`.

### `manual/guides/recovery-scenarios.md`

- Lines 57-63: reframe the `--expect-count` example. Drop "During a
  single-user cutover from an old state file" and `pool.json.old`; the
  flag is now a generic fail-closed guard for any rebuild where the
  operator can name the expected count ahead of time. Pre-record the
  count from your own records / `braid status` output, not from a
  `pool.json.old` artifact.
- Line 73: drop the "previews when the existing `pool.json` is the legacy
  name-keyed shape" clause. The rest of the bullet about healthy UUID-keyed
  refusal stays.

### `tests/cli/braid-discover.py`

The existing test only covers healthy-UUID refusal and read-only preview;
the change in `main.rs` is not exercised end-to-end without folding in the
still-current subtests from the migration test. Move these scenarios in
(adapt to the existing 2-disk fixture by adjusting expected counts).

**Ordering matters.** All scenarios that exercise rebuild paths must run
*before* the first `braid unlock`, so the final unlock subtest can
genuinely prove the rebuilt `pool.json` opens and mounts the pool. If
unlock runs first, the pool is already mounted and a later
`braid unlock` takes the already-mounted no-op path, leaving the rebuild
proof vacuous. Concretely, the file should run in this order:

1. Bare discover preview lists labeled disks + write-hint string (no
   pool.json present yet -- existing subtest).
2. **Corrupt / off-schema preview refusal, byte-for-byte unchanged.**
   Two cases: parseable-but-unrecognized (e.g. `{"unexpected":true}`)
   and unparseable (`not-json-at-all`). Each must hit the "corrupt or
   unreadable -- run `braid discover --write` to rebuild" message and
   leave `pool.json` byte-for-byte unchanged. Modeled on
   `assert_corrupt_pool_json_refuses_preview` in the deleted test.
3. **Name-keyed file is treated as corrupt.** New case (post-cleanup
   behavior): seed a name-keyed pool.json, run bare `discover`, assert
   it hits the corrupt rebuild remediation -- not the old legacy hint
   -- and is byte-for-byte unchanged. This is the integration-level
   pin for the `main.rs` branch collapse and is what the unit test in
   `discover.rs` cannot exercise.
4. **`--expect-count` mismatch refuses and writes nothing.** Seed a
   corrupt pool.json, run `discover --write --expect-count 1` and
   `--expect-count 3` against the 2-disk fixture; assert
   `expected exactly N members, found 2` for each and that
   `pool.json` remains untouched.
5. **`--expect-count` exact match rebuilds.** With a corrupt pool.json
   seeded, `discover --write --expect-count 2` succeeds, writes the
   new UUID-keyed file, and produces the forensic sidecar
   (`pool.json.corrupt-<RFC3339-UTC>`).
6. **Rebuilt pool.json is usable -- unlock succeeds.** This is the
   relocated unlock-and-mount proof from the current test. Runs
   immediately after step 5 so it genuinely opens the rebuilt file.
7. Bare `discover` and `discover --write` both refuse the healthy
   UUID-keyed `pool.json` (existing subtests; stay after unlock).

Each new subtest gets the standard preamble (Intent / Why it exists /
Scenario) per `AGENTS.md`. Drop the migration-specific assertions: legacy
hint string ("legacy name-keyed pool.json detected"), the move-aside
operator workflow, the `is not in UUID-keyed format` (Rust-side)
assertion, and the 3-disk fixture. Also drop the existing intermediate
`discover --write` step that creates `pool.json` from scratch before
unlock; it is now redundant with step 5.

### `tests/cli/braid-discover-migration.py`

- Delete the file entirely.

### `tests/cli/braid-discover-migration.nix`

- Delete the file entirely.

### `flake.nix`

- Delete the `braid-discover-migration` check registration (lines 173-174
  and the `import ./tests/cli/braid-discover-migration.nix { ... }` block
  it expands into).

### `manual/commands/discover.md`

- Lines 39-44: reframe the `--expect-count` example. Drop "During a
  cutover from an old state file"; replace with a generic phrasing such
  as "If you can name the expected member count ahead of time, pass it
  as a fail-closed guard against a detached disk or stray
  braid-labeled disk." The `sudo braid discover --write --expect-count 3`
  shell example stays.

### `docs/luks-unlock.md`

- Lines 161-164 (inside the corrupt-rebuild paragraph): drop "During a
  single-user cutover" framing. Replace with: "If you know the expected
  member count ahead of time, pass `--expect-count <N>` to fail closed
  against a temporarily detached disk or an unrelated braid-labeled disk
  being silently admitted."
- (Already listed) Delete the "legacy name-keyed" subsection at lines
  166-170.

### `cli/src/main.rs` (additional)

- The `DiscoverArgs::expect_count` doc comment (lines 290-296) frames the
  flag as "Used by the LUKS-UUID-as-identity cutover runbook". Rewrite
  the doc comment to describe the flag in generic terms: a fail-closed
  guard for any `discover --write` rebuild where the caller can name the
  expected member count ahead of time. Drop the
  `docs/luks-unlock.md` cutover reference.

### `tests/cli/braid-destroy.py`

- Update Scenario 2 (lines 91-111): keep the test, since `braid-destroy.sh`
  still validates the UUID-keyed shape via its own jq regex. Rename the
  subtest and section comment from "old name-keyed pool.json rejects" to
  "non-UUID-keyed pool.json rejects" and drop the "old name-keyed" mention
  in the file-level comment (lines 6-7). The fixture body and the
  `"is not in UUID-keyed format"` assertion stay -- they exercise the
  shell-side check, which is independent of the Rust legacy detection.

## Verification

1. `just test-rust` -- exercises:
   - The renamed `load_membership_rejects_non_uuid_top_level_keys` test
     (still passes; pins the `LuksUuidMap` deserializer invariant).
   - The renamed
     `non_uuid_keyed_targets_fails_parse_with_remediation_phrase` test
     (still passes; pins the journal `Parse` error wording).
   - The existing `discover_write_rebuilds_corrupt_pool_json` test
     (legacy file now flows through this path).
   - The new test pinning that a name-keyed file now classifies as
     `Corrupt`.
2. `just test-vm braid-destroy braid-discover` -- exercises the destroy
   shell-script regex (renamed subtest) and the expanded discover
   scenarios (corrupt preview refusal, name-keyed-as-corrupt rebuild,
   `--expect-count` over/under/exact, real-binary corrupt rebuild).
3. `cargo build` and `cargo clippy` -- pin no dead-code warnings from
   leftover references to removed variants/functions.
4. Residual-reference grep (final "is anything left?" check). Two
   scoped passes. Notes on form: `rg -E` sets the file encoding, not
   the regex flavor -- ripgrep already supports `|` alternation by
   default, so do not pass `-E`. And plain `name-keyed` text appears
   in current UUID-boundary test comments that defend against
   name-keyed regressions (e.g. `cli/src/remove.rs` and
   `cli/src/remove_missing.rs`), so it cannot be searched repo-wide
   without false positives -- it goes in the scoped pass.

   ```sh
   # Pass A: legacy code identifiers, repo-wide. These should appear
   # only in the legacy code being removed; after this plan, both
   # passes should return zero hits.
   rg -n \
     'NameKeyed|LegacyNameKeyed|NameKeyedPoolJson|is_legacy_name_keyed_shape' \
     cli/ tests/ docs/ manual/ scripts/ flake.nix

   # Pass B: plain "name-keyed" text and cutover/runbook phrasing,
   # only in files this plan edits. Repo-wide `name-keyed` would
   # false-positive on defensive UUID-boundary test comments; bare
   # `migration` would false-positive on unrelated migration notes.
   rg -n 'name-keyed|cutover|old state file|pool\.json\.old' \
     cli/src/discover.rs \
     cli/src/main.rs \
     cli/src/membership.rs \
     cli/src/journal.rs \
     docs/decisions/024-luks-uuid-identity.md \
     docs/luks-unlock.md \
     manual/commands/discover.md \
     manual/guides/troubleshooting.md \
     manual/guides/recovery-scenarios.md \
     flake.nix \
     tests/cli/braid-discover.py \
     tests/cli/braid-destroy.py
   ```

   Both passes should return zero hits, except possibly the
   decision 024 "Rejected Alternatives" section if it survives the
   ADR edits as intentional historical context.

## Critical files

- `cli/src/discover.rs` -- main shape detection and write gates.
- `cli/src/main.rs` -- discover command branch and `DiscoverArgs::expect_count` doc.
- `cli/src/membership.rs` -- load-time rejection tests.
- `cli/src/journal.rs` -- pending-op parse rejection test.
- `docs/decisions/024-luks-uuid-identity.md` -- ADR text.
- `docs/luks-unlock.md` -- operator runbook (legacy section + cutover phrasing).
- `manual/commands/discover.md`, `manual/guides/troubleshooting.md`,
  `manual/guides/recovery-scenarios.md` -- end-user docs (cutover phrasing).
- `flake.nix` -- removes the cutover test from the check set.
- `tests/cli/braid-discover.py` -- absorbs still-current subtests
  (corrupt preview refusal, `--expect-count`, corrupt rebuild,
  name-keyed-as-corrupt).
- `tests/cli/braid-discover-migration.{py,nix}` -- deleted after subtest move.
- `tests/cli/braid-destroy.py` -- scenario 2 reframed.
