# Plan: reproducible fresh-format UUID in dry-run preview

## Context

`braid add --dry-run` and `braid replace --dry-run` render a per-invocation
**random** `--uuid` into the fresh-disk LUKS-format preview line, so two
consecutive dry-runs of the same command print different output.

Root cause: for a `PresentNotLuks` target the planner mints
`LuksUuid::new_v4()` at plan time (`add.rs#build_add_work_plan`,
`replace.rs` derives `new_uuid`) so the journal can record identity before
`cryptsetup luksFormat` runs (ADR-024). That real, random UUID then flows
into the rendered dry-run argv via `render_steps -> CmdRequest::CryptsetupLuksFormat ->
Step::render_dry_run -> to_argv` (`--uuid <random>`).

Why it is worth fixing (Low severity, but real):
- The preview is non-reproducible and the displayed UUID is misleading -- it
  is an authoritative-looking value that is discarded when dry-run returns;
  a later real run mints a *different* one.
- The maintainers already engineered dry-run *ordering* reproducibility for
  exactly this reason (`AddWorkPlan::targets_sorted_by_name`, with the comment
  "UUIDs are random per disk ... effectively random per invocation"). The
  *value* non-determinism is the same class of defect, left unaddressed.

Intended outcome: dry-run preview for a fresh LUKS format shows a fixed,
self-documenting placeholder token (`<generated-at-format-time>`) in the
`--uuid` slot so the output is reproducible and honest that the identity is
minted at execute time. The real run is unchanged and still formats under the
planned UUID.

Rendered form note: `CmdArgs::to_shell_string` uses `shell_words::join`, which
quotes the angle brackets, so the preview line renders as
`--uuid '<generated-at-format-time>'` (quoted), consistent with the
already-quoted `'--key-file=-'` in the same line. The token is kept in
angle-bracket form -- the placeholder signal is worth the harmless quoting; a
shell-safe unquoted token was considered and rejected as a weaker signal.

### Scope (verified by a full preview-nondeterminism sweep)

- **In scope -- exactly two instances:** the fresh (`PresentNotLuks`) arms of
  `add.rs#AddWorkPlan::render_steps` and `replace.rs#ReplaceWorkPlan::render_steps`.
- **Out of scope -- `recover`:** `recover.rs#render_add_pool_mutation_recovery_steps`
  also emits `CryptsetupLuksFormat`, but its UUID comes from the **journal**
  (committed identity) -- reproducible *and* meaningful. Recover must keep
  rendering its real UUID. This is why a blanket change to `to_argv` is wrong.
- **No sibling non-determinism:** the sweep found no other per-invocation value
  (timestamps, PIDs, unsorted map iteration) in any dry-run preview.

## Key constraints (these force the design)

1. `to_argv()` is called by **both** the preview renderer **and** the real
   process spawn (`RealRunner::run -> RealRunner::exec(&request.to_argv())`).
   So `CryptsetupLuksFormat::to_argv` MUST keep emitting the real UUID -- there
   is no safe "one value, two renderings" trick.
2. `LuksUuid` (`types.rs`) is a validated newtype (only `parse`/`new_v4`); it
   cannot hold a non-UUID placeholder string, and it Serializes/Displays its
   inner value, so a nil/sentinel `LuksUuid` would be a latent corruption risk.
3. `Step` is output-only (ADR-022); `execute()` never consumes the rendered
   step -- fresh formats run via `luks.rs#luks_format` with the work-plan
   target's `luks_uuid`. So changing the *rendered* command cannot affect the
   real run.

Conclusion: the preview needs a **distinct value of a distinct type** -- a
preview-only `CmdRequest` variant carrying no UUID, whose `to_argv` renders the
placeholder token. This was chosen over (a) a nil-`LuksUuid` sentinel (punctures
the newtype invariant; an all-zeros UUID is still misleading) and (b) a
pre-rendered display string on `Step` (a "second model" ADR-022 forbids, and it
bypasses `to_argv` shell-quoting and the argv-pin test net).

## Design

Add a preview-only twin of the format command and route the two fresh-format
render sites through it. Keep both argv renderings in lockstep via one shared
arg-vec builder so a future luksFormat flag automatically appears in preview.

## Edit sites

### `cli/src/cmd.rs`

1. **Const + shared builder (DRY seam).** Add a module-level
   `const PREVIEW_LUKS_UUID_PLACEHOLDER: &str = "<generated-at-format-time>";`
   and a private `fn luks_format_argv(uuid_token: &str, label: &LuksLabel,
   extra_opts: &LuksFormatExtraOpts, device: &str) -> CmdArgs` extracted from
   the existing `CryptsetupLuksFormat` arm of `CmdRequest::to_argv`. Preserve
   the load-bearing `--uuid` before `--label` before extras before device
   ordering (and its comment) in the helper.

2. **Real arm refactor (pure).** `CmdRequest::to_argv`'s `CryptsetupLuksFormat`
   arm calls `luks_format_argv(uuid.as_str(), label, extra_opts, device)`. No
   behavior change -- the existing real-variant argv-pin tests must stay green.

3. **New preview variant.** Add to the `CmdRequest` enum (next to
   `CryptsetupLuksFormat`):
   ```rust
   /// Preview-only twin of `CryptsetupLuksFormat`. Carries no `uuid` because
   /// dry-run runs before identity is minted; `to_argv` renders a fixed
   /// placeholder in the `--uuid` slot. NEVER executed: never built by
   /// `luks_format()` or `execute()`, and `RealRunner` hard-errors on it
   /// (see `is_preview_only` guard). See ADR-022 (preview/real divergence).
   CryptsetupLuksFormatPreview { device: String, label: LuksLabel, extra_opts: LuksFormatExtraOpts },
   ```
   Its `to_argv` arm calls `luks_format_argv(PREVIEW_LUKS_UUID_PLACEHOLDER, ...)`.
   Field types are already imported (`cmd.rs:1`); no new imports.

4. **Execution guard (fail-closed runtime backstop) + leave `requires_stdin`
   alone.** The preview variant must never reach a real spawn. Two layers:
   - *Type level:* it is never built by `luks_format()` or `execute()`, and the
     rendered `Step` is output-only (ADR-022).
   - *Runtime backstop:* add a predicate `fn is_preview_only(&self) -> bool` on
     `CmdRequest` (true only for `CryptsetupLuksFormatPreview`) and check it
     FIRST in BOTH `RealRunner::run` and `RealRunner::run_with_stdin`,
     returning `CmdError::Failed("... is preview-only and must never be
     executed")` before any dispatch. This mirrors the existing
     `requires_stdin` routing guard (`cmd.rs#RealRunner`).

   Do NOT add the variant to `requires_stdin`: it is not a stdin request, and a
   `false` there is exactly what makes the runtime guard necessary -- without
   it, `RealRunner::run` falls through to `RealRunner::exec` (which spawns via
   `Command::output()` with **closed stdin**), so
   `cryptsetup luksFormat --batch-mode --key-file=-` would read a zero-length
   key and **format the device under an empty passphrase**. This is the only
   destructive passphrase-class request, and the project bar is
   hard-error-in-all-builds fail-closed (AGENTS.md "Mutation Safety
   Heuristics": residual invariant checks must be hard errors; fail-closed from
   the downstream failure mode) -- a doc comment alone is below that bar.
   (`MockRunner` is out of scope: it returns canned outputs and never spawns.)

No other `CmdRequest` match needs an arm: `to_argv` is the only exhaustive
`match self`; the runner, `MockRunner`, TUI effect mapping, `progress.rs`, and
`preview.rs` all dispatch generically or via wildcards.

### `cli/src/add.rs`

5. **`AddWorkPlan::render_steps`, Fresh arm.** Swap the emitted command from
   `CryptsetupLuksFormat { device, uuid: target.luks_uuid.clone(), label, extra_opts }`
   to `CryptsetupLuksFormatPreview { device, label, extra_opts }` (drop only the
   `uuid` field). The enroll / header-backup / open steps are unchanged.
   `target.luks_uuid` stays in `FreshLuksTarget` -- execute still needs it.
   Add a one-line comment: `// preview variant: real uuid minted at execute; ADR-022`.

### `cli/src/replace.rs`

6. **`ReplaceWorkPlan::render_steps`, `ReplaceTargetPrep::FreshLuks` arm.** Same
   swap: drop `uuid: self.new_uuid.clone()`, emit `CryptsetupLuksFormatPreview`.
   `self.new_uuid` stays (execute path uses it). Same comment.

### `docs/design/decisions/022-dry-run-preview-model.md`

7. Add a new `###` subsection under the **Output contract** section (NOT under
   Decision). The "representative commands" / "`Step` is output-only" language
   this builds on lives in the **Decision** section, so reference it rather than
   appending there. Record the one intentional preview-vs-real divergence: a
   fresh-format identity is minted per-invocation at plan time, so dry-run emits
   `CryptsetupLuksFormatPreview` rendering a fixed
   `--uuid '<generated-at-format-time>'` placeholder (quoted by `shell_words`),
   while the real run uses `CryptsetupLuksFormat` with the journaled identity;
   both share one argv builder; `recover` is excluded because it formats under a
   committed (reproducible, meaningful) journal UUID.

### Untouched on purpose (confirms the seam)

`luks.rs#luks_format` (builds the real variant with the real UUID), `add.rs`
Pass 2 and `replace.rs` execute (call `luks_format` directly, never the rendered
`Step`), and all of `recover.rs`.

## Tests

New:
- **Reproducibility (the core property), one per command.** In `add.rs` and
  `replace.rs` test mods, bind a **single** `StatePaths` (one `test_paths()`,
  one `AddStepsInput` / `ReplaceWorkPlanTestInput`) and call the builder
  **twice on that same borrowed input** (`build_add_work_plan(&runner, &input)`
  / `replace_work_plan_for_test(&input)` -- each call mints a fresh `new_v4()`
  internally), render each via `Step::render_dry_run`, and `assert_eq!` the two
  outputs. **Critical:** do NOT call `test_paths()` twice -- it mints a fresh
  random `tempfile::TempDir` per call, and the header-backup path
  (`luks.rs#luks_header_backup_path`) flows into both the step description and
  the `--header-backup-file` argv, so two different tempdirs would make the
  outputs differ even AFTER the fix (the implementer would wrongly think the fix
  is broken). One `StatePaths` isolates the minted UUID as the only variable and
  matches production (the state dir is fixed there). Fails pre-fix (random
  `--uuid` differs), passes post-fix. At least one of the two should also assert
  the output `.contains("<generated-at-format-time>")` to lock intent, not just
  determinism.
- **Preview argv-pin (`cmd.rs`).** Mirror
  `cryptsetup_luks_format_renders_uuid_label_before_extras_and_device` for the
  preview variant: pin the full slice with `--uuid <generated-at-format-time>`,
  plus a second case with `extras_from(&["--use-random"])` pinning extras land
  after `--label` and before `<device>`.
- **Helper parity (`cmd.rs`).** Build real (`test_uuid(N)`) and preview argv for
  identical device/label/extras; assert they are identical except the token at
  the `--uuid` index. This is the regression guard for the shared builder -- it
  fails if someone adds a flag to one arm but not the other.
- **Preview shell-string pin (`cmd.rs`).** Pin the full rendered shell-string of
  the preview format line via `.to_argv().to_shell_string()`, mirroring
  `to_shell_string_luks_format_with_label`. The expected string contains
  `--uuid '<generated-at-format-time>'` (single-quoted by `shell_words::join`,
  like the sibling `'--key-file=-'`). The argv-pin test above checks the raw
  `Vec<String>` (bare token); this one locks the rendered/quoted form the
  operator actually sees, so the quoting cannot silently drift.
- **Execution guard (`cmd.rs`).** Assert BOTH `RealRunner::run` and
  `RealRunner::run_with_stdin` reject a `CryptsetupLuksFormatPreview` value.
  **Pin the specific guard message** -- `matches!(err, CmdError::Failed(ref msg)
  if msg.contains("preview-only"))`, mirroring `luks_format_run_without_stdin_errors`
  -- NOT a bare `is_err()` / `CmdError::Failed(_)` check. A loose check passes
  even with the guard reverted: without it, `requires_stdin()` is `false`, so
  `run` falls through to `RealRunner::exec`, which returns `Err(Failed(...))` on
  spawn failure (no `cryptsetup` on the macOS test host -- the common case) and
  `Ok(non-zero)` where it spawns; either way a loose assertion is a false
  negative that defeats the "fails on revert" purpose. The `"preview-only"`
  substring is never produced by the fallthrough, so it fails reliably in every
  environment. Use an obviously-nonexistent device string (not a real `/dev/...`)
  so a future guard-regression that *does* reach the spawn cannot touch a real
  device. Runs without spawning because the guard returns before `exec`.

Must stay UNCHANGED (anchor the real-UUID contract; assert in review they are
not edited):
- `add.rs#add_fresh_records_structured_luks_format_request`,
  `replace.rs#cmd_replace_forwards_positive_luks_format_extra_to_request`
  (execute-path; inspect the real `CryptsetupLuksFormat` reaching the runner).
- `cmd.rs` real-variant argv pins
  (`cryptsetup_luks_format_renders_uuid_label_before_extras_and_device`,
  `cryptsetup_luks_format_forwards_non_managed_extras_in_order`) and the
  `requires_stdin` routing tests (`luks_format_run_with*_stdin*`).

Stay green with NO edit (verify, do not "fix"):
- The existing dry-run render tests
  (`add.rs#dry_run_render_fresh_single_disk_bootstrap`,
  `add.rs#dry_run_render_fresh_disk_with_keyfile_orders_backup_after_addkey`,
  `replace.rs#dry_run_render_fresh_disk_live_replace_with_keyfile`) pin
  `--label`/`--pbkdf` but deliberately not `--uuid`, so the format line is
  byte-identical except the unasserted UUID slot.
- `replace.rs#plan_replace_live_preview_*` byte-equivalence tests compare
  `preview().render()` vs `Step::render_dry_run(&preview.steps)` -- both sides
  derive from the same `render_steps()`, equivalent by construction.

## Non-goals

- No broader extraction of the shared fresh-LUKS step *sequence* (format ->
  [enroll] -> header backup -> open) between `add` and `replace`; the downstream
  flows differ and the finding does not motivate it.
- No change to UUID generation timing (ADR-024 plan-time minting is correct).
- `recover` rendering is intentionally left as-is.

## Verification

1. `just test-rust` (CLI unit tests; package `braid-cli`). Expect the new tests
   (reproducibility, preview argv-pin, helper parity, preview shell-string pin,
   execution guard) to pass and all anchored tests to stay green.
2. Manual sanity (optional, on a host/VM with `braid`): run `braid add --dry-run`
   twice against a fresh disk and confirm byte-identical output containing
   `--uuid '<generated-at-format-time>'` (quoted); repeat for
   `braid replace --dry-run`. Confirm `braid recover --dry-run` still shows a
   real journaled UUID.
3. `mdbook build docs` to confirm the ADR-022 edit keeps cross-links valid.

## Ordered sequence

1. `cmd.rs`: add const + `luks_format_argv` helper; refactor the real arm to
   call it. Run `cmd.rs` real-variant pins -- green.
2. `cmd.rs`: add `CryptsetupLuksFormatPreview` + its `to_argv` arm (helper +
   const); add the `is_preview_only` predicate + the fail-closed guard at both
   `RealRunner::run` and `RealRunner::run_with_stdin`. Do not touch
   `requires_stdin`.
3. `cmd.rs`: add preview argv-pin, helper-parity, preview shell-string pin, and
   execution-guard tests. Run `cmd.rs` tests.
4. `add.rs` and `replace.rs`: swap the two fresh-format render arms to the
   preview variant.
5. Add the two reproducibility tests (single `StatePaths`, builder called twice).
6. Run the touched add/replace dry-run render tests + `plan_replace_*_preview_*`
   -- confirm green, unedited.
7. ADR-022 edit.
8. `just test-rust` for the whole CLI crate.
