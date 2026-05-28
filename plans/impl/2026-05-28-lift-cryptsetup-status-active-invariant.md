# Plan: lift the cryptsetup-status parser invariant into the type

## Context

A `/verify-issue` review of a "Low / Correctness" finding -- that
`classify_candidate_mapper` mislabels a particular skip warning as
"mapper is inactive" -- traced back to a deeper problem: the cited
`None` arm at `cli/src/lock.rs:185-191` is **unreachable**. The
parser at `cli/src/parse/cryptsetup_status.rs:64-81` enforces the
invariant that an active mapper always carries a backing device value
(it returns `ParseError::MissingField` otherwise, pinned by the test
at `cryptsetup_status.rs:135-145`), but the type
`CryptsetupStatusOutput { is_active: bool, device: Option<String> }`
does not express that invariant, so every consumer writes a dead
`None` arm. The five sites write that arm *inconsistently* -- two
sites mislabel it "inactive", one treats it as a backing-device
conflict, one as a "null underlying" device, and one as `Unknown` --
which is the underlying class of bug the finding stumbled into.

A second latent issue lurks in the same shape: every consumer also
rejects `Some("(null)")` and most also reject `Some("")` (cryptsetup
prints `device: (null)` when the backing block device has been hot
unplugged; empty is possible from the current `parse_device_line` if
the value side is blank). These checks are duplicated across the
five consumers and applied inconsistently -- e.g. `probe.rs` does
not check empty, so a parser quirk that returns `Some("")` would
get routed through the "present device" path.

The ideal fix is structural: encode the invariant in the type so the
dead arms cannot be written and the null/empty handling has one
authoritative location. This is the established pattern in the
project for parser outputs that have a per-state shape (see
`ScrubState`, `BalanceState`, `ReplaceState` in
`cli/src/parse/types.rs`).

## Outcome

After this refactor:

- `is_active == true && device == None` is unrepresentable.
- `(null)` and empty backing values cannot accidentally be treated
  as a real device path.
- All five consumers (`lock.rs`, `probe.rs`, `probe_mapper_uuid.rs`,
  `luks.rs`, `tui/probe.rs`) match on the same exhaustive enum, so
  their handling of the inactive / null-backing / real-backing
  states converges by construction.

## Type shape

In `cli/src/parse/types.rs` (replacing the current struct at lines
137-140):

```rust
/// Result of `cryptsetup status <mapper>`. The active-vs-inactive
/// split is enforced by the parser: an inactive mapper carries no
/// backing device; an active one always carries a typed backing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CryptsetupStatusOutput {
    Inactive,
    Active { backing: BackingDevice },
}

/// Backing block device reported by an active mapper. Cryptsetup
/// prints `device: (null)` when the underlying block device has
/// been hot-unplugged; braid additionally folds empty or
/// whitespace-only parsed values into `Null` defensively, since
/// `parse_device_line` can yield `""` if the value side of the
/// `device:` line is blank. Folding both into a single `Null`
/// variant prevents consumers from routing either value through
/// the "real path" code path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackingDevice {
    Path(String),
    Null,
}
```

Mirror the derive set used by sibling state enums (`ScrubState`
etc.) for consistency. No `Serialize`/`Deserialize` is required --
the existing struct does not derive them and no snapshot test pins
the JSON shape (confirmed via search of `tests/`).

## Parser change

`cli/src/parse/cryptsetup_status.rs`:

- The two inactive paths (lines 41-44, 52-55) return
  `CryptsetupStatusOutput::Inactive`.
- The active path (lines 78-81) classifies the trimmed device value:
  `"(null)"` or empty becomes `BackingDevice::Null`; anything else
  becomes `BackingDevice::Path(value)`. Then return
  `CryptsetupStatusOutput::Active { backing }`.
- The `MissingField` error path (lines 73-76) is unchanged --
  missing-device-line on active output is still a hard parse error.

Update the four parser unit tests at `cryptsetup_status.rs:84-146`
to destructure the new variants (assert
`matches!(out, CryptsetupStatusOutput::Active { backing: BackingDevice::Path(p) } if p == "/dev/vdb")`
etc.).

## Consumer pattern (apply uniformly)

Every consumer collapses to a three-arm match. The shared shape:

```rust
match parse_cryptsetup_status(&status_raw)? {
    CryptsetupStatusOutput::Inactive => /* per-callsite inactive */,
    CryptsetupStatusOutput::Active { backing: BackingDevice::Null } =>
        /* per-callsite null-backing */,
    CryptsetupStatusOutput::Active { backing: BackingDevice::Path(device) } =>
        /* per-callsite real-device */,
}
```

Each site keeps its own per-arm semantics; only the dispatch shape
changes. Specifically:

- **`cli/src/lock.rs:161-207` (`classify_candidate_mapper`)** --
  `Inactive` returns `Err("mapper is inactive")`. `Null` returns
  `Err("mapper backing device is unavailable (cryptsetup reports null)")`
  (replacing the current pair of arms that print the literal
  device value, since `Null` no longer carries a string). `Path(device)`
  continues to drive the LUKS-UUID probe. The dead "mapper is
  inactive" arm at lines 185-191 -- the finding's headline -- goes
  away with the type.
- **`cli/src/probe.rs:341-368` and `cli/src/probe.rs:434-460`** --
  `Inactive` returns `ProbeError::PoolDevice { detail: "not active" }`.
  The current `None` and `Some("(null)")` arms collapse into the
  single `Null` arm; `Path(device)` keeps the existing present-device
  semantics. This also incidentally fixes the latent gap where the
  current code treats `Some("")` as a real device.
- **`cli/src/probe_mapper_uuid.rs:49-80`** -- `Inactive` and `Null`
  both emit the existing `eprintln!` warn-and-return-false skip with
  a per-state reason string. `Path(device)` drives the UUID probe.
- **`cli/src/luks.rs:846-880` (`check_mapper_ownership`)** --
  `Inactive` returns `Ok(MapperOwnership::Inactive)`. `Null` returns
  `Err(OwnershipError::Conflict { found: None, .. })` (matching the
  existing semantic for the collapsed `None | Some("") | Some("(null)")`
  arm). `Path(device)` drives the rest of the body.
- **`cli/src/tui/probe.rs:34-51`** -- `Inactive` returns
  `(DiskLockState::Locked, None)`. `Null` returns
  `(DiskLockState::Unknown, None)`. `Path(device)` continues into
  the backing-path-resolver flow.

Search the whole `cli` crate (`rg 'CryptsetupStatusOutput|status\.is_active|status\.device'`)
after the rewrite to be sure no other callers survive.

## Tests

- **Parser unit tests** (`cli/src/parse/cryptsetup_status.rs:84-146`) --
  rewrite assertions onto the new enum variants. Add one new test
  asserting that `device: (null)` and `device:    ` (whitespace-only
  value) both parse to `BackingDevice::Null` -- this pins the new
  type-level invariant.
- **Golden tests** (`cli/tests/support/golden_common.rs:187-196`
  and `469-481`) -- rewrite the two assertions onto `matches!`
  patterns over the new enum.
- **Rust unit coverage for the planner** -- the Rust `#[test]`
  functions `full_arm_stranded_mapper_classify_failure_skips_candidate`
  (`cli/src/lock.rs:4597-4643`) and the unaccounted-member
  suppression test (`cli/src/lock.rs:4645-4675`) exercise the
  classify-failure path through `MockRunner` with raw stdout
  strings. Neither constructs `CryptsetupStatusOutput` literals
  directly, so the type refactor only requires the consumer
  match-shape updates -- the tests themselves should keep passing
  unchanged.
- **VM coverage** -- `braid-lock` (`flake.nix:482`) and
  `luks-lock-skipped-no-false-closed` (`flake.nix:502`) are the
  NixOS VM checks that drive lock and stranded-mapper handling
  through real cryptsetup; they validate the refactor at the
  integration boundary. No fixture file changes are needed -- the
  existing `cryptsetup-status-active.txt` and
  `cryptsetup-status-inactive.*` fixtures still parse correctly
  under the new parser logic.

## Critical files

- `cli/src/parse/types.rs` -- type definition (replace struct with
  the two enums).
- `cli/src/parse/cryptsetup_status.rs` -- parser branches and unit
  tests.
- `cli/src/lock.rs` -- `classify_candidate_mapper`.
- `cli/src/probe.rs` -- two consumer sites in pool probing.
- `cli/src/probe_mapper_uuid.rs` -- post-commit close probe.
- `cli/src/luks.rs` -- `check_mapper_ownership`.
- `cli/src/tui/probe.rs` -- TUI disk-lock-state probe.
- `cli/tests/support/golden_common.rs` -- two golden assertions.

## Conventions to reuse

- Derive shape `#[derive(Debug, Clone, PartialEq, Eq)]` on both new
  enums, matching the existing `ScrubState` / `BalanceState` /
  `ReplaceState` pattern in `cli/src/parse/types.rs`.
- Place the new enums adjacent to the existing
  `CryptsetupStatusOutput` location (around line 137) -- the file
  groups parser output types by upstream tool.
- Doc-comment on each top-level item per project convention (see
  `AGENTS.md` "Doc Comments"): the type doc explains the parser
  invariant; `BackingDevice` doc explains the `(null)`/empty
  collapse.

## Commit shape

One commit covering type, parser, all five consumers, and tests.
The project has a "no backwards compatibility" rule (`AGENTS.md`),
and the type change is non-compiling without the consumer updates,
so splitting would require a temporary shim and is rejected.

Suggested message (Conventional Commits, lowercased first word per
`AGENTS.md`):

```
refactor(parse): lift cryptsetup-status active invariant into the type

Replace `CryptsetupStatusOutput { is_active, device: Option<String> }`
with an enum that separates Inactive from Active { backing }, and
fold `(null)` / empty backing values into `BackingDevice::Null`.
The parser already enforced both invariants; encoding them in the
type removes five dead `None` arms across lock, probe,
probe_mapper_uuid, luks, and tui/probe, and fixes a latent gap
where probe.rs would route `Some("")` through the present-device
path.
```

## Verification

1. `just test-rust` -- exercises the parser unit tests, the golden
   tests, and every Rust unit test that touches the consumers,
   including the `full_arm_stranded_mapper_classify_failure_skips_candidate`
   and `full_arm_pass3_classify_failure_suppresses_known_closed_members`
   `#[test]` functions in `cli/src/lock.rs:4604` and `4653` that
   pin the planner's stranded-mapper behavior.
2. `just test-vm braid-lock luks-lock-skipped-no-false-closed` --
   the two NixOS VM checks (registered in `flake.nix:482` and `502`)
   most directly exercising the lock-with-cryptsetup-probe path
   touched by this refactor. `luks-mapper-drift` (flake.nix:492)
   is also adjacent if a broader sweep is desired.
3. `rg 'is_active|status\.device' cli/src` after the rewrite --
   should return zero results outside the new parser and its tests,
   proving every consumer was migrated.
4. `rg '"mapper is inactive"' cli/src` -- should match only the
   `Inactive` arm of `classify_candidate_mapper` (and its
   `probe_mapper_uuid.rs` counterpart), never the impossible
   `None`-was-here site.
5. Do not run `just test-vm` unscoped -- the change has narrow
   blast radius (parser + five direct consumers), so per
   `AGENTS.md` test-scope guidance, focused VM tests plus
   `just test-rust` are sufficient before handing back. The user
   can run the full suite on their side.
