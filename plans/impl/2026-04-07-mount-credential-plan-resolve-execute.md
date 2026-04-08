# Plan: split mount credential into plan → resolve → execute phases

## Context

`mount::Credential<'a>` (`cli/src/mount.rs:38`) currently has three variants
that mix two different concepts:

- `Passphrase { passphrase_stdin, passphrase_file }` — a *source description*,
  read lazily inside `open_and_mount_pool` only when `plan.to_unlock` is
  non-empty.
- `KeyFile(&Path)` — also a source description.
- `InMemoryPassphrase(String)` — *resolved bytes*, owned, exists only because
  `cmd_recover` needs the same passphrase across two `open_and_mount_pool`
  calls (initial mount + the post-resume cycle in `relock_and_remount`).

The eager variant was added in plans/wip/rustling-tumbling-meerkat.md as a
local fix for the recover-twice problem. It works, but the source-vs-resolved
mix is the wrong shape: the laziness rule ("don't prompt when every mapper is
already open") lives buried inside `open_and_mount_pool`, the recover case
needs its own variant for no fundamental reason, and tests must construct
synthetic passphrase files just to feed `read_passphrase` from inside the
mount layer.

This refactor replaces the enum with a three-phase API: `plan_open_pool`
(already exists, read-only) → `resolve_credential` (new helper, **a pure
source-to-credential resolver with no gating logic**) → `execute_open_plan`
(renamed from `open_and_mount_pool`, takes a fully-resolved
`OpenCredential`). Callers compose the three phases explicitly and **own
the decision of when to invoke `resolve_credential`**, because that
decision differs by command:

- `cmd_unlock` resolves only when `plan.to_unlock` is non-empty (the
  existing "no prompt when every mapper is already open" UX rule).
- `cmd_recover` resolves whenever it has a plan (i.e., the pool is not
  already mounted), regardless of `to_unlock`. This is critical: recover
  *always* runs a post-mount relock/remount cycle that closes every LUKS
  mapper and must reopen them, so a credential is needed even if the
  initial plan's `to_unlock` is empty (e.g., all mappers were already
  open and only the btrfs mount step is needed for the first call).
  Today's recover.rs:238-245 already reads the passphrase whenever the
  pool is not mounted, regardless of mapper state — this refactor
  preserves that exactly.

The intended outcome:

- The laziness rule lives at each callsite, where it can differ per
  command. No single helper encodes a rule that's correct for unlock but
  wrong for recover.
- `cmd_recover` reads once into a local and reuses the same owned credential
  across both execute calls — no clone, no second variant, no tempfile.
- `OpenCredential` is owned (no lifetime parameter), holds bytes (not a
  source description), and uses `Zeroizing<String>` so the plaintext is
  scrubbed on drop.
- Test sites construct credentials directly from byte literals instead of
  writing temp files just to satisfy the source-description shape.

The refactor is a behavior-preserving carve at a different joint. It does
not change how LUKS open/verify works, what the recover cycle does, or how
the dry-run/plan path renders steps.

### What this refactor actually provides (and what it does NOT)

**Provides:**
- An explicit per-callsite gate for "should we read a credential now?",
  visible in the local control flow of `cmd_unlock` and `cmd_recover`.
  Today that decision is implicit in the variant choice and the rule
  ("only when `to_unlock` is non-empty") is buried inside
  `open_and_mount_pool`.
- An owned `OpenCredential` reusable across N execute calls (recover's two
  calls become trivial to share).
- Tests that construct credentials from byte literals, decoupling them from
  filesystem fixtures.
- `Zeroizing<String>` for plaintext passphrases — actually scrubbed on drop.
- A *runtime* check in `execute_open_plan` that the credential's
  presence/absence matches the plan's `to_unlock` state, in BOTH
  directions. This catches caller bugs at the boundary; it is not a
  type-system guarantee.

**Does NOT provide (do not claim otherwise):**
- Compile-time enforcement that the credential matches the plan. The
  invariant `cred.is_some() == !plan.to_unlock.is_empty()` is upheld by
  callers and a runtime `MountError::Failed` in `execute_open_plan` —
  not by the type system.
- Any change to the single-passphrase invariant (Principle 4). The verify
  step inside `execute_open_plan` is unchanged.
- Any change to dry-run behavior. `plan_open_pool` + `compile_open_steps`
  is the existing dry-run pipeline; this refactor leaves it alone.
- Any change to recover's prompt-when-not-mounted behavior. Recover
  continues to prompt eagerly whenever the pool is unmounted, even if
  every mapper happens to be open at the moment of probe.

## Approach

### New types in `cli/src/mount.rs`

```rust
use zeroize::Zeroizing;

/// A fully-resolved credential ready to drive `cryptsetup open`.
/// Owned (no lifetime parameter); plaintext is scrubbed on drop.
pub enum OpenCredential {
    Passphrase(Zeroizing<String>),
    KeyFile(PathBuf),
}

/// Where to read a credential from. Mirrors the existing
/// passphrase_stdin/passphrase_file/key_file fields on UnlockParams /
/// RecoverParams. Constructed by callers; consumed by `resolve_credential`.
pub struct CredentialSource<'a> {
    pub passphrase_stdin: bool,
    pub passphrase_file: Option<&'a Path>,
    pub key_file: Option<&'a Path>,
}
```

### New helper: `mount::resolve_credential` (pure resolver, no gating)

```rust
/// Resolve a credential source into an owned, fully-resolved
/// `OpenCredential`. ALWAYS reads — callers decide whether to invoke this
/// (because the "should we prompt now?" rule differs by command:
/// `unlock` skips when `to_unlock` is empty; `recover` always reads when
/// the pool is not yet mounted, because the post-mount cycle will need
/// the credential even if the initial plan does not).
///
/// Resolution order: key_file (if provided) → passphrase (file/stdin/TTY).
pub fn resolve_credential(
    source: &CredentialSource<'_>,
) -> Result<OpenCredential, MountError> {
    if let Some(kf) = source.key_file {
        return Ok(OpenCredential::KeyFile(kf.to_path_buf()));
    }
    let pp = luks::read_passphrase(source.passphrase_file, source.passphrase_stdin)?;
    Ok(OpenCredential::Passphrase(Zeroizing::new(pp)))
}
```

This helper does NOT take an `&OpenPlan`. Encoding the laziness rule
inside the resolver was the central mistake of an earlier draft of this
plan: it baked in unlock's gate ("skip if `to_unlock` is empty"), which
silently regresses recover's all-mappers-open-but-pool-unmounted path.
Recover must read upfront, then carry the credential into the cycle.
Keeping `resolve_credential` pure leaves each caller free to gate as it
needs.

### Rename + reshape: `open_and_mount_pool` → `execute_open_plan`

The renamed function takes a pre-built `&OpenPlan` (no longer calls
`plan_open_pool` itself) and an `Option<&OpenCredential>`. Body:

```rust
pub fn execute_open_plan<R, F>(
    runner: &R,
    fs: &F,
    config: &Config,
    plan: &OpenPlan,
    credential: Option<&OpenCredential>,
    command_hint: &str,    // kept for error messages
) -> Result<bool, MountError>
where R: CommandRunner, F: Filesystem + ?Sized
{
    // Validate credential/plan agreement — STRICTLY, in BOTH directions.
    // Each caller is expected to gate credential presence to match
    // `plan.to_unlock` itself; mismatches mean a caller bug.
    match (credential.is_some(), plan.to_unlock.is_empty()) {
        (false, false) => {
            return Err(MountError::Failed(
                "internal: credential required for unlock but none was provided".into()
            ));
        }
        (true, true) => {
            return Err(MountError::Failed(
                "internal: credential provided but plan has no disks to unlock".into()
            ));
        }
        _ => {}    // (false, true): mount-only path. (true, false): normal unlock.
    }

    // 1. If disks need opening → match credential, call existing helpers.
    if !plan.to_unlock.is_empty() {
        match credential.expect("checked above") {
            OpenCredential::KeyFile(kf) => {
                // Existing keyfile arm body from open_and_mount_pool, verbatim.
                // Calls verify_key_file + ensure_luks_open_with_key_file in a loop.
            }
            OpenCredential::Passphrase(pp) => {
                open_disks_with_passphrase(runner, fs, &plan.to_unlock, pp.as_str())?;
            }
        }
    }

    // 2. btrfs device scan + mkdir + mount — UNCHANGED from current
    //    open_and_mount_pool body (lines 512-548).
    ...
}
```

The strict (true, true) validation is what forces recover to gate the
credential at the *callsite* (see below) rather than passing it
unconditionally. That gating is what makes the refactor faithful to its
goal of making laziness visible.

`open_disks_with_passphrase` (already private at `cli/src/mount.rs:352`) is
unchanged. The keyfile loop body moves verbatim from the existing
`Credential::KeyFile` arm at `cli/src/mount.rs:442-498`.

The current `open_and_mount_pool` calls `plan_open_pool` internally as its
first step (mount.rs:434). After the refactor, that call moves OUT to the
caller. There is no convenience wrapper that does plan+execute together —
the whole point is to make the phases visible.

### Delete `Credential<'a>`

Once all callers and tests migrate, the enum at `cli/src/mount.rs:38` is
removed entirely. No compatibility shim — per AGENTS.md "no backwards
compatibility".

### Update `cli/src/unlock.rs`

`cmd_unlock` becomes:

```rust
pub fn cmd_unlock<R, F>(...) -> Result<(), UnlockError> {
    preflight::check_no_pending_operation(...)?;

    let plan = mount::plan_open_pool(
        runner, fs, params.config, params.membership,
        params.allow_degraded, "unlock",
    )?;

    if params.dry_run {
        if let Some(ref p) = plan {
            let steps = mount::compile_open_steps(p, params.config.mount_point(), params.key_file);
            Step::print_dry_run(&steps);
        }
        return Ok(());
    }

    let Some(plan) = plan else { return Ok(()); };  // already mounted

    // Unlock-specific gate: only resolve a credential if there is something
    // to unlock. Preserves the "no prompt when every mapper is already open"
    // UX rule today implicit inside open_and_mount_pool.
    let credential = if plan.to_unlock.is_empty() {
        None
    } else {
        let source = mount::CredentialSource {
            passphrase_stdin: params.passphrase_stdin,
            passphrase_file: params.passphrase_file,
            key_file: params.key_file,
        };
        Some(mount::resolve_credential(&source)?)
    };

    let mounted = mount::execute_open_plan(
        runner, fs, params.config, &plan, credential.as_ref(), "unlock",
    )?;

    if !mounted { return Ok(()); }  // defensive — plan should have caught this

    // ... existing post-mount enrichment + paused-balance warning, unchanged ...
}
```

The branching that today builds `Credential::KeyFile` vs
`Credential::Passphrase` at `cli/src/unlock.rs:63-70` is gone — that
key-file-vs-passphrase dispatch moves into `resolve_credential`. The
laziness gate (`if plan.to_unlock.is_empty()`) lives at this callsite,
explicit and visible.

### Update `cli/src/recover.rs`

Recover's gating rule is different from unlock's: **always resolve
upfront if the pool is not already mounted**, because the post-mount
relock cycle will close every mapper and need to reopen them, regardless
of the initial plan's `to_unlock`. This matches what recover.rs:238-245
does today (gate on `already_mounted`, not on `to_unlock`).

```rust
// ...existing journal/membership setup unchanged...

// Build the plan ONCE. plan_open_pool returns None when already mounted —
// this replaces the manual MountpointCheck at recover.rs:231-236.
let plan = mount::plan_open_pool(
    runner, fs, params.config, &union, params.allow_degraded, "recover",
)?;

// Recover-specific gate: resolve a credential ANY time we have a plan
// (i.e. the pool is not already mounted). This is eager on purpose —
// the relock cycle below will need it even if the initial plan has
// to_unlock.is_empty() (every mapper already open).
let credential = match plan.as_ref() {
    Some(_) => {
        let source = mount::CredentialSource {
            passphrase_stdin: params.passphrase_stdin,
            passphrase_file: params.passphrase_file,
            key_file: None,    // recover does not expose --key-file today
        };
        Some(mount::resolve_credential(&source)?)
    }
    None => None,    // already mounted, no cycle, no credential needed
};

// Initial mount. Pass the credential to execute_open_plan ONLY if the
// initial plan needs it — execute_open_plan strict-validates both
// directions. The resolved credential stays alive in `credential` for
// the cycle below either way.
let just_mounted = match plan.as_ref() {
    Some(p) => {
        let cred_for_initial = if p.to_unlock.is_empty() {
            None
        } else {
            credential.as_ref()
        };
        mount::execute_open_plan(
            runner, fs, params.config, p, cred_for_initial, "recover",
        ).map_err(|e| { /* existing bootstrap-recovery error massaging at lines 268-307 */ })?
    }
    None => false,    // already mounted
};

// Post-resume cycle. Unchanged in spirit; relock_and_remount now takes
// the credential by reference instead of a &str.
if just_mounted {
    wait_for_kernel_replace_to_finish(runner, params.config.mount_point());
    let cred = credential.as_ref()
        .expect("just_mounted implies plan was Some and credential was resolved");
    relock_and_remount(runner, fs, params.config, &union, params.allow_degraded, cred)?;
}

// ... rest of cmd_recover unchanged ...
```

`relock_and_remount`'s signature changes from `passphrase: &str` (recover.rs:477)
to `credential: &OpenCredential`. Inside, the existing umount + scan-forget +
LUKS-close loop is unchanged. The final remount step calls `plan_open_pool`
fresh (because the mappers are now closed → `to_unlock` will be all disks)
and then `execute_open_plan` with the borrowed credential:

```rust
// At the end of relock_and_remount (replacing the current line 544 call):
let cycle_plan = mount::plan_open_pool(
    runner, fs, config, membership, allow_degraded, "recover",
)?
.ok_or_else(|| RecoverError::Failed(
    "recover remount cycle: pool already mounted after umount?".into()
))?;
// The cycle ALWAYS has work to do (we just closed every mapper), so
// to_unlock is non-empty and we always pass the credential.
mount::execute_open_plan(
    runner, fs, config, &cycle_plan, Some(credential), "recover",
).map_err(|e| RecoverError::Failed(format!("recover remount cycle: re-mount: {e}")))?;
```

The clone at the current `Credential::InMemoryPassphrase(passphrase.to_owned())`
(recover.rs:549) is gone — the credential is borrowed.

### Update test sites in `cli/src/mount.rs`

All 16 test functions that construct a `Credential::*` value need updating
(line numbers per Explore: 663, 699, 788, 878, 1259, 1351, 1392, 1457, 1525,
1595, 1689, 2047, 2107, 2162, 2235, 2320). The mechanical translation per
test:

1. Replace `open_and_mount_pool(..., Credential::Passphrase{...} | Credential::KeyFile(...), ...)`
   with two calls:
   ```rust
   let plan = plan_open_pool(&runner, &fs, &config, &membership, allow_degraded, "unlock")
       .unwrap()
       .expect("plan should not be None for this test");
   execute_open_plan(&runner, &fs, &config, &plan,
       Some(&OpenCredential::Passphrase(Zeroizing::new("testpass".into()))),
       "unlock")
   ```
   The temp-file `NamedTempFile` setup that exists in many tests *only* to
   satisfy `Credential::Passphrase { passphrase_file: Some(tmp.path()) }` can
   be deleted entirely.

2. Tests using `Credential::KeyFile(kf.path())` keep their `NamedTempFile`
   for the keyfile (cryptsetup actually reads it), but switch to
   `OpenCredential::KeyFile(kf.path().to_path_buf())`.

3. The `mount_already_mounted_returns_false` test (mount.rs:663) currently
   passes a never-used `Credential::Passphrase{stdin:false, file:None}`.
   After the refactor, the same test calls `plan_open_pool` (which returns
   `Ok(None)` because mountpoint check succeeds) and never reaches
   `execute_open_plan`. The test asserts on the plan being None.

Tests in `unlock.rs` and `recover.rs` do NOT construct `Credential` directly
— they call `cmd_unlock` / `cmd_recover` which take params structs. Those
tests are unaffected (the params structs are unchanged).

### Update `cli/Cargo.toml`

Add `zeroize = { version = "1", features = ["derive"] }` (only the basic
feature is needed for `Zeroizing<String>`; no derive necessary). Verify it
isn't already pulled in transitively before adding.

### What is NOT changing

- `luks::read_passphrase`, `luks::verify_passphrase`,
  `luks::ensure_luks_open`, `luks::ensure_luks_open_with_key_file`,
  `luks::verify_key_file` — signatures and bodies unchanged.
- `mount::plan_open_pool`, `mount::compile_open_steps`,
  `mount::OpenPlan` — unchanged.
- `mount::open_disks_with_passphrase` — unchanged (the helper that
  rustling-tumbling-meerkat.md introduced is still the right shape).
- `mount::explain_open_failure` and the keyfile-arm error massaging — same
  bodies, just moved into the new `execute_open_plan` shell.
- The post-mount enrichment in `cmd_unlock` (refresh pool.json metadata,
  paused-balance warning) — unchanged.
- The journal/recovery flow in `cmd_recover` outside the mount-call lines
  (probe → recovered membership → save pool.json → clear journal) —
  unchanged.
- `wait_for_kernel_replace_to_finish` and the umount + scan-forget +
  cryptsetup close loop in `relock_and_remount` — unchanged. Only the
  final remount call signature changes.
- `add.rs`, `replace.rs`, `enroll_key_file.rs` — these never used
  `Credential` and call `read_passphrase` directly. Untouched.
- `tests/` (NixOS VM tests) — Python files that drive the CLI via
  subprocess; no Rust enum references.
- `UnlockParams` and `RecoverParams` field shapes — keeping the loose
  `passphrase_stdin`/`passphrase_file`/`key_file` fields and constructing
  `CredentialSource` at the callsite. Embedding `CredentialSource` into the
  param structs is a separate (out-of-scope) cleanup.
- `main.rs` dispatch — unchanged. Same params construction, same call
  signatures.

## Critical files

- `cli/src/mount.rs`
  - `cli/src/mount.rs:38` — delete `Credential<'a>` enum, add `OpenCredential`
    and `CredentialSource<'a>`
  - `cli/src/mount.rs:113` — `OpenPlan` (no change, used by new API)
  - `cli/src/mount.rs:352` — `open_disks_with_passphrase` (no change)
  - `cli/src/mount.rs:422` — `open_and_mount_pool` → rename to
    `execute_open_plan`, drop the internal `plan_open_pool` call, take
    `&OpenPlan` + `Option<&OpenCredential>`
  - Add new public `mount::resolve_credential(source: &CredentialSource)
    -> Result<OpenCredential, MountError>` — pure resolver, takes NO plan,
    always reads, callers gate
  - 16 test sites listed above — mechanical credential-construction updates
- `cli/src/unlock.rs:31-104` — `cmd_unlock` reshaped to plan → optional
  resolve → execute. Resolution gated at the callsite by
  `if plan.to_unlock.is_empty()` (preserves the no-prompt-when-all-open
  rule).
- `cli/src/recover.rs`
  - `cli/src/recover.rs:231-258` — drop the manual `MountpointCheck` and
    the conditional `read_passphrase`. Replace with: `plan_open_pool`
    once, then `resolve_credential` once whenever the plan is `Some`
    (i.e. pool not already mounted), regardless of `to_unlock`. Then at
    the initial `execute_open_plan` callsite, pass the credential only
    when `p.to_unlock` is non-empty (`cred_for_initial`), so
    `execute_open_plan`'s strict (true, true) check is satisfied. The
    resolved credential stays alive in a local for the cycle below.
  - `cli/src/recover.rs:340-354` — pass `&OpenCredential` instead of
    `&str` to `relock_and_remount`
  - `cli/src/recover.rs:471-555` — `relock_and_remount` signature change
    (`passphrase: &str` → `credential: &OpenCredential`); the final
    `open_and_mount_pool` call at line 544 becomes a `plan_open_pool` +
    `execute_open_plan` pair, always passing `Some(credential)` because
    the cycle just closed every mapper and the cycle's plan will always
    have `to_unlock` non-empty
- `cli/Cargo.toml` — add `zeroize` to `[dependencies]`

## Functions/utilities to reuse

- `luks::read_passphrase` (`cli/src/luks.rs:49`) — called from inside
  `resolve_credential`. Signature unchanged.
- `luks::verify_passphrase` (`cli/src/luks.rs:171`) — called from
  `open_disks_with_passphrase`. Unchanged.
- `luks::ensure_luks_open` / `ensure_luks_open_with_key_file`
  (`cli/src/luks.rs:302, 356`) — called from inside the new
  `execute_open_plan` body via the existing keyfile arm and
  `open_disks_with_passphrase` helper. Unchanged.
- `luks::verify_key_file` (`cli/src/luks.rs:386`) — called from the
  keyfile arm of `execute_open_plan`. Unchanged.
- `mount::plan_open_pool` (`cli/src/mount.rs:129`) — now called explicitly
  by `cmd_unlock`, `cmd_recover`, `relock_and_remount`, and the test
  sites. Already public, signature unchanged.
- `mount::compile_open_steps` (`cli/src/mount.rs:238`) — used only by the
  dry-run path in `cmd_unlock`. Unchanged.
- `mount::open_disks_with_passphrase` (`cli/src/mount.rs:352`) — the
  passphrase arm of the new `execute_open_plan` calls this helper exactly
  as the current `Credential::Passphrase` arm does.
- `mount::explain_open_failure` (`cli/src/mount.rs:320`) — used inside the
  keyfile arm of `execute_open_plan`. Unchanged.

## Verification

1. **Build:** `cargo build -p braid-cli` — clean compile. The lifetime
   parameter on `Credential<'a>` is gone, so any forgotten reference will
   surface as an unresolved-name error.

2. **Unit tests:** `cargo test -p braid-cli --lib` — all existing tests
   pass. The 16 mount.rs tests have rewritten construction; behavior is
   identical. The unlock.rs and recover.rs test surfaces are unchanged
   (they go through `cmd_*` which still take the same params).

3. **Targeted scopes:**
   - `cargo test -p braid-cli --lib mount::` — confirms execute_open_plan
     behaviour matches the old open_and_mount_pool across the 16 tests.
   - `cargo test -p braid-cli --lib unlock::` — confirms the lazy-read
     path is preserved (unlock with all-mappers-already-open does not
     prompt; degraded mount tests still pass).
   - `cargo test -p braid-cli --lib recover::` — confirms the recover
     cycle still works without the temporary passphrase clone.

4. **Targeted regression test (REQUIRED, not optional):** add a new
   `recover::` unit test that exercises the all-mappers-open-but-pool-
   unmounted path. This is the exact regression the earlier draft of this
   refactor introduced and that wider unit tests would NOT catch (because
   today's recover tests use the `Credential::InMemoryPassphrase` shape,
   which trivially carries the passphrase across the cycle). The test
   must be added in the same commit as the refactor, not as a follow-up.

   **Test name:** `recover_with_all_mappers_open_still_resolves_credential_for_cycle`
   (or similar — name should make the regression scenario unambiguous).

   **Intent / Why / Scenario block** (per AGENTS.md test convention):
   - **Intent:** Recover must resolve a credential up-front whenever the
     pool is not already mounted, even if every LUKS mapper happens to
     be open at probe time, because the post-mount relock/remount cycle
     closes every mapper and must reopen them.
   - **Why it exists:** A natural-looking refactor (gating
     `resolve_credential` on `plan.to_unlock.is_empty()`) silently
     breaks this path. Unlock is allowed to skip the prompt; recover is
     not. Without this test the refactor can pass `cargo test` while
     leaving recover unable to complete its cycle in production.
   - **Scenario:** A 2-disk pool whose LUKS mappers were manually opened
     by an operator (`cryptsetup open` outside braid). The pool is NOT
     mounted at `/mnt/storage`. A pending-op journal exists from a
     previous interrupted operation. The operator runs `braid recover`.
     Expected: passphrase is read once via the supplied
     `passphrase_file`, the initial mount succeeds with `to_unlock`
     empty (mount-only path), the relock cycle closes both mappers,
     reopens them with the same passphrase, remounts, and recovery
     completes.

   **MockRunner setup (concrete):**
   - `MountpointCheck { /mnt/storage }` → exit 1 (not mounted) — for the
     initial `plan_open_pool`.
   - `CryptsetupLuksUuid` for both disks → ok (both LUKS).
   - **mapper_open probe** (whichever request the existing
     `probe::probe_config_disk` issues) → reports `mapper_open: true`
     for both disks. This is what makes `plan.to_unlock` empty in the
     initial plan.
   - `BtrfsDeviceScanAll` → ok (initial mount).
   - `Mount { /dev/mapper/braid-disk1, /mnt/storage }` → ok (the
     mount-only path of the initial `execute_open_plan`).
   - **Probe pool + balance status** (the existing probe sequence
     cmd_recover runs after just_mounted) → return a healthy single-op
     pool consistent with the journal so the remount cycle is reached.
   - `BtrfsReplaceStatus` → `None` (no replace running, so
     `wait_for_kernel_replace_to_finish` exits immediately).
   - **Cycle umount + scan-forget + close mappers** (sequence at
     recover.rs:483-538): all ok.
   - **Cycle re-plan**: `MountpointCheck` → exit 1, `CryptsetupLuksUuid`
     for both disks → ok with `mapper_open: false` (we just closed them).
   - **Cycle credential use**: `CryptsetupTestPassphrase` against the
     first disk with the passphrase bytes from the file → ok. This is
     the line that fails if the credential was never resolved — the
     mock for it is what catches the regression.
   - `CryptsetupLuksOpen` for both disks with the same passphrase →
     ok (mappers reopen).
   - `BtrfsDeviceScanAll` + `Mount` → ok (cycle remount).
   - Final probe + journal-clear sequence: ok.

   **Assertions:**
   1. `cmd_recover(...).unwrap()` succeeds end-to-end.
   2. The mock recorded a `CryptsetupTestPassphrase` (or
      `CryptsetupLuksOpen` with stdin) call carrying the exact
      passphrase bytes from the test's `passphrase_file`. This proves
      the credential was actually resolved and reached the cycle —
      not silently dropped or replaced with `None`. Use the
      `MockRunner`'s stdin-recording API (whatever
      `with_output_stdin` produces; existing recover tests already use
      this pattern, e.g. unlock.rs:256-281).
   3. The post-cycle `Mount` request was issued, confirming the cycle
      ran to completion.

   **What this test does NOT cover (out of scope for this refactor):**
   the actual kernel-resumed `dev_replace` race that drove the
   meerkat/sharded-drifting-beaver work. That continues to live in the
   VM repro test. This is a pure-Rust regression guard for the
   credential-flow shape only.

5. **Parser canary:** `just test-parsers` — unaffected (no parser changes),
   but run as a smoke test against live tool output.

6. **VM tests:** `just test-vm` — full suite. The most relevant scenarios:
   - `unlock_*` tests (basic unlock, degraded, key-file unlock, already-open
     mappers don't prompt)
   - `recover_*` tests, especially the dev_replace post-resume cycle that
     drove the rustling-tumbling-meerkat.md fix in the first place. Per
     `feedback_vm_verify_kernel_async_assumptions.md`, do NOT declare the
     refactor done until the recover-cycle VM repro is green — unit tests
     can pass while the kernel state machine still loses.

7. **Repro test:** `just test-repro repro-recover-mid-replace` (or whichever
   repro test covers the dev_replace cycle) — must remain green. This is
   the load-bearing scenario the eager-passphrase variant was added to
   support.

8. **Manual zeroize sanity check:** add a one-shot test that constructs an
   `OpenCredential::Passphrase`, drops it, and confirms (via a length
   assertion on a wrapper, not a memory probe — just confirm the
   `Zeroizing<String>` shape compiles and drops cleanly). This is enough
   to ensure the dependency wires up; it doesn't need to be a security
   guarantee.

## Out of scope (deliberately not in this plan)

- Embedding `CredentialSource` into `UnlockParams` / `RecoverParams` (a
  separate cleanup; would touch `main.rs` dispatch).
- Adding `--key-file` support to `cmd_recover` (recover currently has no
  keyfile path; the `CredentialSource` constructed in `cmd_recover` always
  passes `key_file: None`).
- Restructuring tests to separately test plan vs execute. The 16 mount.rs
  tests stay end-to-end; this refactor just rewrites how they pass the
  credential.
- Adopting `Zeroizing<String>` anywhere outside the `OpenCredential` type.
  Other passphrase locals in `add.rs`/`replace.rs`/`enroll_key_file.rs` keep
  using plain `String` for now — converting them is a separate sweep.
- Removing `tempfile` from runtime `dependencies`. The
  rustling-tumbling-meerkat.md plan already moved it back to dev-deps; this
  refactor doesn't re-touch Cargo.toml beyond adding `zeroize`.
