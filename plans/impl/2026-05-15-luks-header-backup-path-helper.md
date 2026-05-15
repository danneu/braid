# Plan: collapse `<luks_headers_dir>/<mapper>.luksheader` path duplication

## Context

The `<luks_headers_dir>/<mapper>.luksheader` filename convention is
hand-rolled at **10 production sites** across 5 files
(`enroll_key_file.rs`, `luks.rs`, `add.rs`, `replace.rs`, `recover.rs`).
The original verify-issue finding surfaced only 2 of them
(`enroll_key_file.rs:391-393` and `luks.rs:490`); investigation uncovered
8 more.

Why this matters now -- not as a bug, but as a maintainability hazard:
- The dry-run/preview vs. execute drift the finding warned about is not
  hypothetical -- it exists today between every planner site
  (`add.rs`/`replace.rs`/`enroll_key_file.rs` Step construction) and the
  writer (`luks.rs:490`), and between the journal replay
  (`recover.rs:816-818`, `862-864`) and the writer. Each pair must be
  read-checked by hand to confirm they still produce the same path.
- Patching only the 2 cited sites leaves 8 other independent owners of
  the same formula. The drift risk migrates rather than goes away.

Scope: this plan centralizes only the *current* mapper-name convention
(`<headers_dir>/<MapperName>.luksheader`). It does not anticipate
identity changes to the filename (e.g. encoding `LuksUuid` for
disambiguation). `MapperName` is presentation/argv identity, not
persistent identity (`cli/src/types.rs:304`); any filename change that
adopts persistent identity would expand the helper signature and ripple
to every caller. Defer that to a separate plan once the desired
filename identity is settled.

Intended outcome: one helper, every production site routes through it.
Zero behavioral change today.

## Design

### Helper

A single free function in `cli/src/luks.rs`, placed alongside
`backup_luks_header_to`:

```rust
/// Single source of truth for the on-disk filename convention every
/// LUKS-header-mutating command must agree on. Planner sites
/// (`add`/`replace`/`enroll`/recover replay) and the writer
/// (`backup_luks_header_to`) all derive the final path from this
/// helper, so dry-run preview, execute, and journal replay cannot
/// drift apart on the mapper-name convention.
pub(crate) fn luks_header_backup_path(
    headers_dir: &Path,
    mapper: &MapperName,
) -> PathBuf {
    headers_dir.join(format!("{}.luksheader", mapper.0))
}
```

**Why a free function (not a `StatePaths` method):**
`cli/src/recover.rs:256`'s `RecoverWorkPlan` caches only
`luks_headers_dir: PathBuf` -- it does not hold `&StatePaths`. A free
function taking `&Path` covers all 10 sites uniformly without
restructuring the recover plan. A `StatePaths` method would either
bifurcate the API (two ways to do one thing) or force a recover-plan
refactor unrelated to this change.

**Why `&MapperName`:** The newtype already exists at
`cli/src/types.rs:309` and every call site has a `MapperName` in scope
(`mn`, `mapper`, `new_mapper`). Locks in type safety. The ripple cost
(see below) is contained inside `luks.rs`.

### Signature ripple

`backup_luks_header_to`, `backup_luks_header`, and
`backup_luks_header_post_mutation` (`cli/src/luks.rs:479`, `:516`,
`:537`) currently take `mapper: &str`. Change all three to
`mapper: &MapperName`. Callers update from `&mn.0` to `&mn`.

Affected callers (already have `MapperName` in scope, just pass it by
reference instead of unwrapping):
- `cli/src/add.rs:1133`, `:1213`
- `cli/src/enroll_key_file.rs:311`
- `cli/src/replace.rs:659`, `:727`
- `cli/src/recover.rs:2567`, `:2634`, `:3054`, `:3121`
- Test sites in `cli/src/luks.rs:1202`, `:1246` (the `mapper` local
  there will need to become a `MapperName`; trivial).

### `backup_luks_header_to` body

`cli/src/luks.rs:479-513`. Replace lines 490-491:

```rust
// before:
let backup_path = dir.join(format!("{mapper}.luksheader"));
let tmp_path = dir.join(format!("{mapper}.luksheader.tmp"));

// after:
let backup_path = luks_header_backup_path(dir, mapper);
let tmp_path = {
    let mut t = backup_path.clone().into_os_string();
    t.push(".tmp");
    PathBuf::from(t)
};
```

`tmp_path` derives from `backup_path` (rather than re-formatting
independently) so the writer's atomic-rename pair cannot disagree on
the mapper-name convention.

### Planner / replay site replacements

Each of the 8 remaining sites swaps inline `dir.join(format!(...))` for
`luks_header_backup_path(...)`:

| File:line | Before | After |
| --- | --- | --- |
| `enroll_key_file.rs:391-393` | `paths.luks_headers_dir().join(format!("{}.luksheader", mn.0))` | `luks_header_backup_path(&paths.luks_headers_dir(), &mn)` |
| `add.rs:1818-1821` | same shape with `input.paths.luks_headers_dir()` and `mn` | `luks_header_backup_path(&input.paths.luks_headers_dir(), &mn)` |
| `add.rs:1884-1887` | same | same |
| `add.rs:1935-1938` | same | same |
| `replace.rs:1518-1521` | with `new_mapper` (a `MapperName`) | `luks_header_backup_path(&input.paths.luks_headers_dir(), &new_mapper)` |
| `replace.rs:1526-1529` | same | same |
| `recover.rs:816-818` | `plan.luks_headers_dir.join(format!("{}.luksheader", mapper.0))` | `luks_header_backup_path(&plan.luks_headers_dir, &mapper)` |
| `recover.rs:862-864` | same | same |

## Out of scope

- **Test fixture literals** -- strings like
  `"braid-disk1.luksheader"` in `tests/storage/luks-header-backup.py`,
  `tests/cli/braid-add-enroll.py`, `tests/cli/braid-doctor.py`,
  `tests/cli/braid-unlock.py`, etc. These assert on the on-disk
  convention from the *outside*, not on the helper output. They stay
  literal -- if the convention later changes, they update at the same
  time as the helper.
- **Unit-test path-literal assertions** in `cli/src/luks.rs:1185`,
  `:1230`, `:1251` (`backup_luks_header_post_mutation_*` tests). They
  pin the writer's externally observable convention; staying literal
  guards the helper itself.
- **`StatePaths` API** -- no method added. The free function suffices.
- **`recover.rs`'s `RecoverWorkPlan` struct** -- the cached
  `luks_headers_dir: PathBuf` field stays as-is. Touching it is
  unrelated cleanup.
- **`header_backup_advisories_in`** (`cli/src/luks.rs:1090`) -- matches
  by extension only (`ext == "luksheader"`, line 1096); unaffected.
- **doctor / status / unlock messaging invariants** -- those forbid
  *user-facing strings* from referencing `/var/lib/braid/luks-headers/`
  or `.luksheader` (per `docs/luks-unlock.md`). This refactor doesn't
  add any new user-facing strings; the invariant is unaffected.

## Files Modified

- `cli/src/luks.rs` -- add helper; update `backup_luks_header_to` body;
  three signature changes (`&str` -> `&MapperName`); test sites at
  `:1202`, `:1246` adapt.
- `cli/src/enroll_key_file.rs` -- 1 planner site + 1 caller update (line
  311).
- `cli/src/add.rs` -- 3 planner sites + 2 caller updates (`:1133`,
  `:1213`).
- `cli/src/replace.rs` -- 2 planner sites + 2 caller updates (`:659`,
  `:727`).
- `cli/src/recover.rs` -- 2 replay sites + 4 caller updates (`:2567`,
  `:2634`, `:3054`, `:3121`).

## New unit test

Add to the `cli/src/luks.rs` test module (existing convention; pure
function, no `CommandRunner`). Preamble per
[`docs/testing.md`](docs/testing.md):

```rust
// Intent: the helper produces <dir>/<mapper>.luksheader and nothing
//   else, so every callsite that routes through it stays in lockstep
//   with `backup_luks_header_to`'s writer.
// Why it exists: 10 production sites currently hand-roll this formula;
//   this test pins the helper's output at the unit level so a missed
//   call site or a typo in the formula fails fast.
// Scenario: a planner site and the writer derive the same path for
//   `braid-disk1`; verify the formula directly.
#[test]
fn luks_header_backup_path_combines_dir_and_mapper_with_luksheader_ext() {
    let dir = Path::new("/var/lib/braid/luks-headers");
    let mapper = MapperName("braid-disk1".to_owned());
    assert_eq!(
        luks_header_backup_path(dir, &mapper),
        PathBuf::from("/var/lib/braid/luks-headers/braid-disk1.luksheader"),
    );
}
```

One test is sufficient. The formula is a single `format!` line, so
there is nothing to vary across cases. Caller-integration is covered
separately: the sanity grep (Verification step 4) proves every
production site routes through the helper, and the VM tests
(Verification step 3) exercise the signature-rippled callers at
runtime.

## Verification

Run in order:

1. **Compile** -- `cargo build --package braid-cli` (or `just test-rust`,
   which compiles first). Catches any signature-ripple miss.
2. **Rust unit tests** -- `just test-rust`. Exercises the new helper
   test, the existing `backup_luks_header_post_mutation_*` tests at
   `cli/src/luks.rs:1178+`, and the `advisory_*` tests at `:2700+`. All
   should pass unchanged in behavior.
3. **VM end-to-end coverage** -- runtime exercise of the
   `backup_luks_header*` callers after the `&str` -> `&MapperName`
   signature ripple. (Path-construction correctness is already pinned
   by the helper unit test and the sanity grep below; these VM tests
   are belt-and-suspenders that the callers still run.)
   - `just test-vm luks-header-backup` -- writer + on-disk filename via
     `tests/storage/luks-header-backup.py`.
   - `just test-vm braid-add-enroll add-enroll-recoverable` -- `add.rs`
     callers (`:1133`, `:1213`).
   - `just test-vm replace-live-disk` -- `replace.rs:659` (FreshLuks
     replace branch); the other replace branch is covered below. Note
     `tests/cli/replace-live-disk.py` does not currently assert on the
     produced backup path itself, so this run only catches caller-side
     regressions (signature, command argv, error wiring), not a silent
     wrong-path write. The unit test + sanity grep are the real path
     guards.
   - `just test-vm replace-enroll-existing-luks
     recover-replace-existing-luks-enroll` -- `replace.rs:727`
     (ExistingLuks) and `recover.rs` journal-replay callers (`:2567`,
     `:2634`, `:3054`, `:3121`).
4. **Sanity grep** -- after the refactor, `rg -n
   '\.join\(format!\("\{[^}]*\}\.luksheader"' cli/src/` should return
   **exactly one hit**: the helper definition in `cli/src/luks.rs`. Any
   additional hit is a missed call site that still hand-rolls the
   formula.

## Risk

Behavioral risk is zero: identical formula at every site, no logic
change. The only failure mode is a missed call site (caught by the
sanity grep in step 4) or a signature-ripple miss (caught by the
compile in step 1).
