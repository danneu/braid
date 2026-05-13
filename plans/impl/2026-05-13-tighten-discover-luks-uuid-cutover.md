# Plan: tighten `braid discover` for the LUKS-UUID cutover

## Context

The LUKS-UUID-as-identity refactor (this branch) changed `pool.json` from
name-keyed to UUID-keyed. Operators with a pre-migration pool (e.g. the
3x12TB pool running master on another host) need to push the new binary
and re-derive `pool.json` with `braid discover --write`. The audit of
`cli/src/discover.rs` and `main.rs::Commands::Discover` surfaced three
issues that block a safe cutover:

- **M1** — `--expect-count N` is a minimum check (`actual < expected`),
  not the exact-match semantics its name implies. An extra braid-labeled
  disk (recovery drive, leftover from a prior install) is silently
  admitted into the new `pool.json`.
- **M2** — Bare `braid discover` (no `--write`) refuses when `pool.json`
  exists, with the error `pool.json already exists ... use 'braid add'`.
  During migration the operator has a legacy-shape `pool.json` and wants
  to preview what discover sees before moving it aside. The current
  refusal forces them to either run `--write` blind or `cryptsetup
  luksDump` every disk by hand. `braid status` is also dead on the old
  shape so there is no inspection path.
- **M3** — No end-to-end VM test exercises the migration cutover.
  Unit tests cover each gate in isolation against a synthetic blob; no
  integration test boots a real multi-disk LUKS fixture with a seeded
  legacy `pool.json` and walks the migration runbook.

These fixes are small, scoped to discover, and ship with the same PR
that introduces the legacy-shape gate.

## Out of scope

- **L1 (`added_at`/`devid` drop)** — confirmed informational only;
  `added_at` is a lazy-stamped "first observed live" timestamp, never
  read by operational logic. `devid` re-enriches on next mount. No
  change.
- L2/L3/L4 — separate small hardenings; not blockers for the cutover.

## Changes

### 1. M1: `--expect-count` exact-equality

**File:** `cli/src/discover.rs`

- Line 161-167: rewrite the `DiscoverWriteError::ExpectCountUnmet` doc
  comment + `#[error(...)]` string to exact-equality phrasing:
  ```
  "discover refusing to write pool.json: expected exactly {expected}
   members, found {actual} -- check that all intended pool members are
   attached and readable, and that no unrelated braid-labeled disks are
   attached, then retry"
  ```
- Line 510-515: change `if actual < expected` to `if actual != expected`.
- Lines 479-481: rewrite the `write_discovered_membership` doc
  comment's `expected_count` paragraph. The current text says "the
  gate refuses if the produced membership has fewer than
  `expected_count` members" -- update to "the gate refuses if the
  produced membership count is not exactly `expected_count`" so the
  internal contract description matches the new symmetric behavior
  (and the renamed unit tests below).

**User-facing doc + help updates (M1):**

- `cli/src/main.rs:293-298` — rewrite the `#[arg(...)] expect_count`
  clap doc comment. Replace "fewer than N members" framing with the
  exact-equality contract: the flag now refuses on under- *and*
  over-count. Wording should mention the over-count case explicitly
  (extra braid-labeled disk attached).
- `manual/commands/discover.md:39-44` — update the cutover variation
  prose to say "fail closed if the discovered member count is not
  exactly N" instead of "fewer than the expected member count".
- `manual/commands/discover.md:51` — update the Flags table row for
  `--expect-count <N>` from "refuse to write if fewer than `N` members
  are discovered" to "refuse to write if the discovered member count is
  not exactly `N`".
- `manual/commands/discover.md:72` — update the Safety-checks bullet
  ("With `--expect-count`, refuses to write if discovery produces
  fewer members than requested.") to the exact-equality framing,
  matching the Flags-table update above.
- `docs/luks-unlock.md:148-152` — update the cutover paragraph to
  describe the symmetric safety: under-count blocks loose-cable /
  udev-race partial attach; over-count blocks an unrelated
  braid-labeled disk being silently admitted.

**Test updates:**

- `cli/src/discover.rs:1538-1574` (`discover_write_refuses_when_below_expected_count`):
  rename to `discover_write_refuses_when_count_mismatches_below` and
  update the wording-pin assertion at line 1566 to the new "expected
  exactly N" string.
- Add a new test
  `discover_write_refuses_when_count_mismatches_above`: seed two
  members, pass `--expect-count 1`, assert `ExpectCountUnmet` with
  `expected=1, actual=2` and the new wording, assert `pool.json` is not
  written.

### 2. M2: bare `discover` migration preview

**File:** `cli/src/discover.rs`

Extract the legacy-shape sniff currently inlined at
`write_discovered_membership` (lines 498-508) into a `pub` helper so the
`main.rs` binary (a separate crate from the `braid-cli` lib -- see
`cli/Cargo.toml:6-8` for the `[[bin]]` declaration alongside the
implicit `[lib]`) can reuse the same logic without duplicating the JSON
walk. `pub(crate)` is insufficient here -- the binary's `Commands::Discover`
arm at `cli/src/main.rs:732` already accesses discover items via
`braid_cli::discover::...`, which is cross-crate.

```rust
/// Classifies an existing pool.json by shape for discover's gating.
/// Returns `LegacyNameKeyed` when the file parses to a JSON object with
/// a `disks` object whose keys include at least one non-UUID -- the
/// pre-LUKS-UUID-identity shape. Returns `Other` for any
/// new-UUID-keyed file or any shape the gate cannot positively
/// classify; the caller decides whether `Other` is safe to overwrite.
/// Returns `Missing` for ENOENT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolJsonShape { Missing, LegacyNameKeyed, Other }

pub fn classify_pool_json(path: &Path) -> PoolJsonShape { ... }
```

`Copy` is required because the `main.rs` arm both `match`es on `shape`
and re-reads it via `matches!(shape, ...)` for the hint branch.
`PartialEq` supports the `== PoolJsonShape::LegacyNameKeyed` comparison
inside `write_discovered_membership`. `Debug` is used for error
diagnostics during development.

Then:

- Inside `write_discovered_membership`: replace the inlined gate with
  `if classify_pool_json(&pool_json_path) == PoolJsonShape::LegacyNameKeyed
   { return Err(NameKeyedPoolJson { ... }) }`. Behavior unchanged.

**File:** `cli/src/main.rs` lines 715-730

Replace the unconditional refusal with shape-aware branching, executed
before the `discover_pool_members` call. Note the cross-crate import:
both items resolve under `braid_cli::discover::` because the binary
is a separate crate from the lib (see helper section above).

```rust
let pool_json = paths.pool_json();
let shape = braid_cli::discover::classify_pool_json(&pool_json);
if !args.write {
    match shape {
        braid_cli::discover::PoolJsonShape::Missing
        | braid_cli::discover::PoolJsonShape::LegacyNameKeyed => {
            // Preview mode: scan and print, no write.
            // For LegacyNameKeyed, emit a migration hint before
            // the scan output.
            if matches!(shape, braid_cli::discover::PoolJsonShape::LegacyNameKeyed) {
                eprintln!(
                    "note: legacy name-keyed pool.json detected at {} -- \
                     this is the pre-migration shape. Run 'braid discover --write \
                     --expect-count N' after moving it aside (see docs/luks-unlock.md).",
                    pool_json.display()
                );
            }
            // fall through to the normal scan + print path
        }
        braid_cli::discover::PoolJsonShape::Other => {
            // New-UUID-keyed or unrecognized shape: keep the original refusal.
            print_cli_error(&format!(
                "pool.json already exists at {} -- use 'braid add' to add disks",
                pool_json.display()
            ));
            std::process::exit(1);
        }
    }
}
```

The `--write` path is unchanged: it still calls
`write_discovered_membership`, which still refuses on
`LegacyNameKeyed` via the same helper.

**User-facing doc updates (M2):**

- `manual/commands/discover.md:56` — replace step 2 ("Refuses if
  `pool.json` already exists (use `braid add` instead).") with the
  shape-aware behavior: refuses only for new UUID-keyed or otherwise
  unrecognized shapes; legacy name-keyed `pool.json` runs in
  read-only preview mode with a migration hint.
- `manual/guides/recovery-scenarios.md:73` — replace "`discover`
  refuses to run if pool.json already exists. Remove it first if it
  exists but is wrong." with the same shape-aware framing: bare
  `discover` previews when the existing file is the legacy
  name-keyed shape; for any other existing shape the operator must
  remove the file first.
- `manual/commands/discover.md:70` — replace the Safety-checks bullet
  "Refuses if `pool.json` already exists." with the shape-aware
  contract: refuses only when the existing `pool.json` is new
  UUID-keyed or otherwise unrecognized; legacy name-keyed runs as a
  read-only preview.
- `manual/guides/troubleshooting.md:37` — rewrite the note
  ("`discover` refuses to run if pool.json already exists. If
  pool.json exists but is wrong, remove it first:") to match: bare
  `discover` previews when the existing file is the legacy
  name-keyed shape, otherwise the operator must remove it first.
  Keep the `sudo rm /var/lib/braid/pool.json` example since that's
  still the right instruction for the new-shape case.

Skipped: `manual/book/` is the generated mdbook output (HTML +
search index, see `manual/book/book.toml` and the surrounding
`.html` artifacts); edits there would be overwritten on next build.

### 3. M3: end-to-end VM migration test

**New file:** `tests/cli/braid-discover-migration.nix`

Mirror `tests/cli/braid-discover.nix` structurally:

- Pass `diskNames = ["disk1" "disk2" "disk3"]` to
  `tests/module/lib/initrd-fixture.nix` (3 disks, deterministic UUIDs
  `11111111-...`, `22222222-...`, `33333333-...` per
  `tests/module/lib/initrd-fixture.nix:100-104`).
- `virtualisation.emptyDiskImages` with three 256MB drives named
  `disk1`/`disk2`/`disk3` (matching the `virtio-disk{n}` by-id
  convention).
- `testScript = builtins.readFile ./braid-discover-migration.py;`.

**New file:** `tests/cli/braid-discover-migration.py`

Follow the
`tests/cli/braid-destroy.py:49-51` `write_pool_json(contents)` helper
pattern for seeding state into `/var/lib/braid/pool.json`. The test
walks the cutover runbook:

1. **Setup:** assert the initrd fixture left no `pool.json`; seed a
   legacy name-keyed `pool.json` mimicking master's shape:
   ```json
   {"disks":{"disk1":{"by_id":"/dev/disk/by-id/virtio-disk1","luks_uuid":"11...","devid":1,"added_at":"2024-01-01T00:00:00Z"},
             "disk2":{...},
             "disk3":{...}}}
   ```

2. **Bare discover under legacy shape (M2):** run `braid discover
   2>&1`. Assert exit 0, output contains all three disk names, the
   migration hint string `"legacy name-keyed pool.json detected"` and
   `"braid discover --write --expect-count"`, and that `pool.json` is
   byte-for-byte unchanged.

3. **`--write` refuses on legacy shape:** run `braid discover --write
   2>&1`. Assert failure, output contains `"is not in UUID-keyed
   format"`, `pool.json` unchanged.

4. **Operator moves pool.json aside:** `mv /var/lib/braid/pool.json
   /var/lib/braid/pool.json.legacy`.

5. **`--write --expect-count 2` (M1 over-count):** run with
   `expect-count 2`. Assert failure with `"expected exactly 2 members,
   found 3"`. Assert `/var/lib/braid/pool.json` was not created.

6. **`--write --expect-count 4` (M1 under-count, unchanged):** run with
   `expect-count 4`. Assert failure with `"expected exactly 4 members,
   found 3"`. Assert `/var/lib/braid/pool.json` was not created.

7. **`--write --expect-count 3` (happy path):** assert success and
   stderr `"pool membership written to /var/lib/braid/pool.json"`.
   `cat pool.json`, JSON-parse, assert keys == `{"11111111-...",
   "22222222-...", "33333333-..."}` and each value's `name` matches the
   expected disk.

8. **Unlock proof:** `echo -n 'testpassphrase' | braid unlock
   --passphrase-stdin`, then `mountpoint /mnt/storage`.

9. **Post-migration bare-discover (regression guard, `Other` =
   new-UUID-keyed):** with the new UUID-keyed `pool.json` in place,
   run `braid discover` and assert it fails with the existing
   `"pool.json already exists at ... -- use 'braid add'"` wording.
   Pins that M2's branching preserves the day-2 behavior for new-shape
   `pool.json`.

10. **Unrecognized-shape refusal (regression guard, `Other` =
    unrecognized):** seed `/var/lib/braid/pool.json` with a payload
    the classifier cannot positively identify -- e.g. `{"unexpected":
    true}` (valid JSON, no `disks` key) or invalid JSON like
    `not-json-at-all`. Run bare `braid discover` and assert the same
    `"pool.json already exists at ... -- use 'braid add'"` refusal
    fires, and that the seeded file is byte-for-byte unchanged.
    Without this subtest a classifier regression that mis-routed
    unrecognized shapes into preview mode would not fail any test.
    Run twice (once per payload) so both the unparseable-JSON and
    parseable-but-no-disks paths are covered.

**File:** `flake.nix` lines 141-145

Register the test in the `checksFor` attrset alongside `braid-discover`:

```nix
braid-discover-migration = pkgs.testers.nixosTest (
  import ./tests/cli/braid-discover-migration.nix {
    braid = linuxCrane.braid;
  }
);
```

## Critical files

- `cli/src/discover.rs` — M1 error string + comparison; extract
  `classify_pool_json` helper (`pub` + `Copy`/`PartialEq`/`Eq`/`Debug`
  derives for cross-crate use from `main.rs`).
- `cli/src/main.rs` — M2 shape-aware bare-discover branching; updated
  `expect_count` clap doc comment for M1.
- `manual/commands/discover.md` — M1 flag-table + cutover prose; M2
  "under the hood" step rewording.
- `manual/guides/recovery-scenarios.md` — M2 bare-discover note
  rewording.
- `manual/guides/troubleshooting.md` — M2 bare-discover note
  rewording.
- `docs/luks-unlock.md` — M1 cutover paragraph: symmetric under/over
  safety.
- `tests/cli/braid-discover-migration.{nix,py}` — M3 new VM test.
- `flake.nix` — register the new check.

## Existing patterns reused

- `tests/cli/braid-destroy.py:49-51` — `write_pool_json` helper for
  seeding legacy `pool.json` shapes into the VM.
- `tests/module/lib/initrd-fixture.nix:100-104` — deterministic LUKS
  UUIDs for predictable post-migration assertions.
- `tests/cli/braid-discover.nix` — structural template for the new
  test's NixOS module.
- `cli/src/discover.rs:498-508` — existing schema sniff to extract into
  the shared helper.
- `cli/src/discover.rs:1538-1574` — model for the new
  `discover_write_refuses_when_count_mismatches_above` unit test.

## Verification

- `just test-rust` — runs all CLI unit tests including the new
  over-count case and the updated wording assertion.
- `just test-vm braid-discover braid-discover-migration` — runs the
  existing discover test (unchanged) and the new migration test.
- Quick manual sanity on the actual remote 3x12TB pool (after binary
  push):
  1. `sudo braid discover` -- should print the 3 disks with the migration
     hint, leave `pool.json` untouched.
  2. `sudo braid discover --write` -- should refuse with NameKeyedPoolJson.
  3. `sudo mv /var/lib/braid/pool.json /var/lib/braid/pool.json.legacy`
  4. `sudo braid discover --write --expect-count 3` -- success.
  5. `sudo braid status` and `sudo braid unlock` confirm pool is
     usable.

## Non-goals / explicitly skipped

- Forwarding `added_at` / `devid` from the legacy `pool.json` into the
  new file (L1) -- confirmed cosmetic, `devid` re-derives on next
  mount, `added_at` has no operational reader.
- Tightening the gate to refuse any non-new-shape `pool.json` (L2) --
  separate hardening; the current legacy-shape-only gate is intentional
  to keep the migration ergonomic.
- Distinguishing `cryptsetup isLuks` exit codes (L3) and pool.json
  backup sidecar on `--write` (L4) -- separate items, no migration
  blocker.
