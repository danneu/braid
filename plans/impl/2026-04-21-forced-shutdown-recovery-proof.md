# Forced-Shutdown Recovery Proof

Derived from: `plans/wip/purrfect-honking-rabin.md` (the original monolithic
UPS-integration plan). This file is one of three follow-ons:

- `plans/wip/ups-v1-safety-core.md` -- smallest shippable safety feature
  set behind `braid.ups.enable`. **Must ship before this plan's VM matrix
  runs, because the VM tests rely on the UPS-driven `SHUTDOWNCMD` wiring
  to simulate forced shutdown.** The recover-code audit (Pre-M11) can
  start in parallel.
- `plans/wip/ups-observability-ux.md` -- rich parser, TUI, doctor.
  Independent of this plan; either order of landing is fine.
- `plans/wip/forced-shutdown-recovery-proof.md` (this file) -- `braid
  recover` audit + the per-mutation VM matrix that proves mid-mutation
  power loss is survivable.

## Scope

Prove that abrupt shutdown during a pool-mutating operation -- `braid
replace`, `braid remove`, `braid remove-missing`, or `braid add`'s post-add
balance phase -- is a recoverable state. Shipping this plan satisfies
ADR 020's "mid-mutation power loss is a supported recovery case" claim,
which is the last gate for flipping ADR 020 to `Status: Active`.

Framing matters: **this is generic forced-shutdown / recover hardening**,
not a UPS-specific concern. UPS low-battery is one motivating trigger (and
is what the VM tests use, because Plan 1 already wires `SHUTDOWNCMD =
systemctl poweroff` through dummy-ups + `upsrw`). Other forced-shutdown
triggers -- kernel panic, OOM kill of the mutation process, a yanked
power cord, a manual `poweroff -f` -- should produce the same post-reboot
recovery behavior. The audit in Pre-M11 evaluates recover paths against
the general "previous boot was interrupted mid-mutation" case, not against
a UPS-specific scenario.

Concretely this plan delivers:

1. An audit of [`cli/src/recover.rs`](../../cli/src/recover.rs) and the
   journal-replay paths for each mutation class, identifying gaps.
2. Any recovery-code remediation the audit turns up -- landed as its own
   milestone, not folded into the VM-test PRs.
3. A shared dummy-ups test harness at
   [`tests/module/lib/ups-fixture.nix`](../../tests/module/lib/ups-fixture.nix)
   that provisions the test-only `upsrw` credential and centralizes the
   driver-mode invariants.
4. Four VM tests, one per mutation class, that trigger a forced shutdown
   mid-mutation, reboot, run `braid recover`, and assert the pool is
   restored without manual intervention.
5. Flipping [`docs/decisions/020-ups-integration.md`](../../docs/decisions/020-ups-integration.md)
   to `Status: Active` once the matrix passes.

## Non-goals

- **Alert-model integration for UPS events.** Still deferred to a future
  ADR. Unchanged from Plan 1 / Plan 2.
- **New forced-shutdown triggers beyond the UPS path.** Adding kernel-
  panic injection or `poweroff -f` harness tests is valuable but a
  separate future plan. This plan frames the audit generically but uses
  the one already-wired trigger (UPS LB via `upsrw`) for test execution.
- **Expanding the rich `parse_upsc` model or adding TUI polish.** Those
  are `plans/wip/ups-observability-ux.md`'s work.
- **Backwards compatibility for recover-code changes.** braid is
  unreleased software (see `AGENTS.md`). If the audit requires changes
  to journal entry shapes or recover semantics, they land in place;
  old journal files on-disk do not need migration paths.

## Dependencies

- `plans/wip/ups-v1-safety-core.md` -- landing its M5 (`SHUTDOWNCMD =
  systemctl poweroff`) and M1 module skeleton is a hard prerequisite for
  the VM matrix. The Pre-M11 audit can start in parallel because it
  reads existing code paths.
- `docs/decisions/018-systemd-lifecycle.md` (`Active`) -- read before
  touching recover / journal semantics, especially because
  `braid-online.service`'s `ExecStop` is the clean-shutdown hinge that
  this plan's tests prove runs under UPS LB.
- `docs/decisions/020-ups-integration.md` (`Draft`) -- this plan is the
  final gate before `Active`. Open Question 1 ("Recovery-proof for
  mid-mutation power loss") is the question this plan answers.
- Local references:
  - `reference/nut/clients/upsrw.c` -- authoritative for `SET`-action
    requirements and the `-s 'ups.status=OB LB'` syntax.
  - `reference/nut/docs/man/dummy-ups.txt:90,100` -- `dummy-once` vs.
    `dummy-loop` semantics. Load-bearing for the shared fixture.
  - `reference/nut/conf/upsd.users.sample` -- authoritative for user
    schema, especially `actions = [ "SET" ]` on the test-only credential.
- Journal / recover code paths in `cli/src/recover.rs`,
  `cli/src/journal.rs`, `cli/src/add.rs`, `cli/src/remove.rs`,
  `cli/src/remove_missing.rs`, `cli/src/replace.rs`.

## Milestones

The audit and any recovery-code remediation land before the VM matrix.
The matrix tests are cheap when the recover path is already right; they
are painful to iterate on when the recover path has gaps and failing
tests conflate "recover is broken" with "test harness is broken."

### Pre-M11 -- `braid recover` audit

Read [`cli/src/recover.rs`](../../cli/src/recover.rs) and the
journal-replay call sites. For each mutation class -- `replace`,
`remove`, `remove-missing`, `add` -- identify precisely what happens
when the previous boot was interrupted partway through. The
boundary conditions to trace:

- **`replace`**: What if `btrfs replace` was in progress on reboot? Is
  `btrfs replace status` consulted, and if so how does recover decide
  resume vs. abort vs. no-op?
- **`remove`**: What if `btrfs device remove` was interrupted? Is the
  device still a member of the pool on the next boot? Does recover
  detect the partial state and re-run / finish?
- **`remove-missing`**: What if the conditional `maybe_restore_raid1`
  soft balance was running when shutdown hit? Does recover detect the
  soft-balance state and resume? Is the journal entry idempotent?
- **`add`**: What if `pool_balance_raid1` was running mid-add? Does
  recover see the new device, see the incomplete balance, and do the
  right thing?

Deliverable of this milestone: a short findings document recorded
directly in this plan (append below the milestone list when done), with
one of two verdicts:

- **No gap, proceed.** Justify each mutation class against its journal
  entry and the expected reboot-recovery flow.
- **Gap list.** Concrete list of code paths to add or change, each with a
  named journal state and the recover behavior required.

If the verdict is "gap list," land the remediation as **M1 (Recovery
remediation)** below before any VM test runs. Gaps here are the single
biggest risk to flipping the ADR Active; surfacing them as failing VM
tests wastes VM cycles and conflates separate concerns.

**Verify:** Audit output appended to this file; if any gaps exist, the
corresponding M1 work is scheduled before the matrix runs.

### M1 -- Recovery-code remediation (conditional on Pre-M11 findings)

Only exists if Pre-M11 identified gaps. Otherwise skipped with a note
in the audit findings.

For each identified gap:

- Extend or fix the relevant recover path in `cli/src/recover.rs`.
- If new journal states are needed, update
  `cli/src/journal.rs` and the mutation entry-point modules
  (`add.rs`, `remove.rs`, etc.).
- Add `MockRunner`-backed unit tests covering the new recover behavior
  before touching VM tests.

**Verify:** `just test-rust` green. Unit tests exercise each newly
handled journal state. Code changes match the gap list exactly -- no
drive-by refactors.

### M2 -- Shared dummy-ups test harness

Create [`tests/module/lib/ups-fixture.nix`](../../tests/module/lib/ups-fixture.nix).
One module imported by each of the four matrix tests, plus (in a
follow-up refactor) the existing `ups-lb-clean-shutdown` test from
Plan 1.

The harness is responsible for:

- Enabling `braid.ups.enable = true` with a named UPS.
- Configuring the `dummy-ups` driver in **`dummy-once` mode** with a
  `.dev` file (per `reference/nut/docs/man/dummy-ups.txt:90,100`). This
  is load-bearing: `dummy-once` loads the `.dev` file into memory once
  and preserves subsequent `upsrw` writes; `dummy-loop` would re-read
  the file and overwrite in-memory `upsrw` changes before `upsmon`
  reacts to the critical state. The harness includes an inline comment
  calling this out, because the failure mode is silent if the wrong
  mode is chosen.
- Provisioning a **test-only** second upsd user (e.g. `testops`) with
  `actions = [ "SET" ]` so `upsrw` can drive state changes. Per
  `reference/nut/docs/man/upsd.users.txt:78`, `SET` is only required by
  `upsrw` clients -- the production upsmon credential (created by
  `braid-ups-secrets.service` in Plan 1) must stay minimal with no
  `actions`. The harness makes this distinction explicit in a comment
  and exposes the `testops` credential to the calling test via a
  known-path env or output variable.
- Providing a helper snippet the `.py` tests can use to drive state:
  ```
  upsrw -s 'ups.status=OB LB' -u testops -p <pass> <upsname>@localhost
  ```
  The `-s` flag and quoted value are required so `OB LB` is parsed as
  one multi-flag status, not two argv tokens (see
  `reference/nut/clients/upsrw.c`).
- Any shared service / timer ordering the matrix tests need.

**Verify:** The harness module evaluates cleanly; a minimal importing
test can import it, unlock a pool, set `OB LB` via `upsrw`, and observe
the pool shutting down. (This is essentially the Plan 1 `ups-lb-clean-
shutdown` scenario reproduced through the shared harness; after this
milestone lands, the Plan 1 test can be refactored onto the harness as
a small cleanup -- either in this plan or as a follow-up.)

### M3 -- VM test: LB during `braid replace` (original M11)

- Create [`tests/module/ups-lb-during-replace.nix`](../../tests/module/ups-lb-during-replace.nix)
  and sibling `.py`. Import the harness from M2.
- Block comment per `AGENTS.md` test convention:
  - **Intent** -- verify that a forced shutdown during an active `btrfs
    replace` is a recoverable state: post-reboot, `braid recover` either
    completes the replace or cleanly resumes it, with no manual
    intervention.
  - **Why** -- ADR 020's guarantee (1) and Open Question 1 depend on
    this. Without this proof, the "supported recovery case" claim is
    unbacked.
  - **Scenario** -- operator ran `braid replace <old> <new>` during a
    prolonged outage; the UPS fired LB mid-replace; host powered off;
    operator boots the NAS the next morning.
- Test body:
  1. Boot the VM with the harness.
  2. Stage the pool with enough data that `braid replace <old> <new>`
     takes ~30s wall-clock to finish -- large enough to reliably catch
     the in-flight state, small enough to keep the test fast.
  3. Start `braid replace <old> <new>` asynchronously from the test
     harness.
  4. Once `btrfs replace status` shows non-zero progress, drive
     `upsrw -s 'ups.status=OB LB' -u testops -p <pass>
     <upsname>@localhost`.
  5. Wait for the host to power off.
  6. Reboot.
  7. Run `braid recover`.
  8. Assert:
     - `braid recover` exit code 0, no diagnostics printed.
     - Pool mounts cleanly.
     - `btrfs device stats <mount>` reports zero errors.
     - No orphaned LUKS mappers (`lsblk` shows no stale mappings).
     - Journal entry for the replace is either complete or cleanly
       resumed (not left in an intermediate "pending" state with no
       owning process).
     - Replace either has finished (final device layout matches the
       requested replacement) or is in a legitimate resumable state
       (`btrfs replace status` reports a cancellable-but-resumable
       state).

**Verify:** `just test-vm ups-lb-during-replace` passes.

### M4 -- VM test: LB during `braid remove` (original M12)

Same shape as M3, but for `braid remove <disk>`.

- Create `tests/module/ups-lb-during-remove.{nix,py}`.
- Block comment: Intent = "recoverable state after LB during
  `btrfs device remove`"; Why = "ADR 020 Open Q1 coverage for the
  remove class"; Scenario = "operator ran `braid remove` during an
  outage."
- Test body mirrors M3 but with `braid remove <disk>` as the triggered
  mutation. The mutation must produce enough data movement to be
  interruptible (staging mirrors M3).

**Verify:** `just test-vm ups-lb-during-remove` passes.

### M5 -- VM test: LB during `braid remove-missing` (original M13)

Same shape. Targets the conditional `maybe_restore_raid1` soft-balance
phase because that is the slow phase where LB interruption is most
plausible.

- Create `tests/module/ups-lb-during-remove-missing.{nix,py}`.
- Block comment: Intent = "recoverable state after LB during the
  `maybe_restore_raid1` soft balance"; Why = "ADR 020 Open Q1 coverage
  for the remove-missing class"; Scenario = "disk physically failed,
  operator started `braid remove-missing`, outage hit mid-rebalance."
- Test stages one simulated missing device, runs `braid remove-missing`
  with enough existing data to drive the soft balance, and triggers
  `OB LB` during the balance.

**Verify:** `just test-vm ups-lb-during-remove-missing` passes.

### M6 -- VM test: LB during `braid add` balance (original M14)

- Create `tests/module/ups-lb-during-balanced-add.{nix,py}`.
- Block comment: Intent = "recoverable state after LB during the
  `pool_balance_raid1` phase of `braid add`"; Why = "ADR 020 Open Q1
  coverage for the balanced-add class"; Scenario = "operator added a
  new disk during an outage, UPS fired LB during the post-add balance."
- Test runs `braid add <new-disk>` with enough existing data that the
  post-add balance is interruptible, and triggers `OB LB` during the
  balance phase (not the initial `btrfs device add` step, which is
  fast).

**Verify:** `just test-vm ups-lb-during-balanced-add` passes.

### M7 -- Flip ADR 020 to `Active`

Only after M3-M6 all pass.

- Modify
  [`docs/decisions/020-ups-integration.md`](../../docs/decisions/020-ups-integration.md):
  - Flip `Status: Draft` to `Status: Active`.
  - Resolve Open Question 1 ("Recovery-proof for mid-mutation power
    loss") with a reference to this plan's matrix and the four passing
    VM tests.
  - Resolve Open Question 2 ("Shutdown ordering for ordinary mounted
    operation") with a reference to Plan 1's M7 (ordinary-operation
    clean-shutdown VM test).
  - Resolve Open Question 3 ("Battery-low threshold") with either "the
    default threshold is sufficient -- M7 passed without remediation"
    or with the raised-threshold value documented from Plan 1's M7b.
- Modify [`docs/index.md`](../../docs/index.md) -- refresh the
  summary line for ADR 020 to reflect `Active` status and the closed
  questions. If a follow-up ADR for `AlertCause` persistence
  semantics is scheduled, add a placeholder summary line pointing at
  it.

**Verify:** ADR 020 now reads `Status: Active`; `docs/index.md`
reflects that; no remaining Open Questions block in the ADR.

## Critical files

**Tests:**
- Create `tests/module/lib/ups-fixture.nix` (shared harness).
- Create `tests/module/ups-lb-during-replace.{nix,py}`.
- Create `tests/module/ups-lb-during-remove.{nix,py}`.
- Create `tests/module/ups-lb-during-remove-missing.{nix,py}`.
- Create `tests/module/ups-lb-during-balanced-add.{nix,py}`.
- Optional follow-up refactor: update
  `tests/module/ups-lb-clean-shutdown.{nix,py}` (from Plan 1) to
  import the shared harness.

**CLI (Rust, conditional on audit findings):**
- Modify `cli/src/recover.rs` (remediation for any identified gaps).
- Modify `cli/src/journal.rs` (if new states are needed).
- Modify `cli/src/add.rs`, `cli/src/remove.rs`,
  `cli/src/remove_missing.rs`, `cli/src/replace.rs` (if journal
  writes need to change).

**Docs:**
- Modify `docs/decisions/020-ups-integration.md` -- Status flip +
  Open Questions resolved.
- Modify `docs/index.md` -- summary-line refresh for ADR 020.

## Verification

**Unit tests (`just test-rust`):**
- Any new `MockRunner`-backed tests from M1 (recovery-code
  remediation). No new unit tests are expected if the audit finds no
  gaps, but the existing `cli/src/recover.rs` unit tests must stay
  green.

**VM tests (`just test-vm`):**
- `ups-lb-during-replace` (M3).
- `ups-lb-during-remove` (M4).
- `ups-lb-during-remove-missing` (M5).
- `ups-lb-during-balanced-add` (M6).

All four must pass before M7 (ADR flip). Failing any of them means
either the recover path has a new gap, the harness is wrong, or the
test scenario is wrong -- investigate the root cause; do not flip the
ADR on three-of-four.

**Manual smoke on real hardware:**
- Hook the target NAS to a real UPS.
- Start a `braid replace` on the real pool with enough data to take
  several minutes.
- Kill utility power at the wall. Let the UPS drain to LB.
- Confirm the host shuts down cleanly; boot back up; run `braid
  recover`; assert the pool is clean and the replace is either
  complete or cleanly resumed.
- Optional: rehearse the same scenario by pulling the NAS's power
  cord mid-replace (bypasses the UPS path; validates that the
  recover guarantee holds for non-UPS forced shutdown too, which is
  the generic-framing point of this plan).

## Risks

1. **Recovery gap in `cli/src/recover.rs`.** Unknown until Pre-M11
   is done. If a gap exists, it surfaces as either a concrete M1 work
   item (good: fix-once-up-front) or as a failing M3-M6 VM test (bad:
   conflates "recover broken" with "test broken"). Mitigation: run
   Pre-M11 first and take its verdict seriously. The ADR's
   "mid-mutation power loss is a supported recovery case" claim is
   only honest after the audit + any remediation; signing off without
   the audit is an integrity problem, not just a scheduling one.

2. **Dummy-ups driver-mode gotcha.** Using `dummy-loop` instead of
   `dummy-once` silently breaks the harness: `upsrw` writes are
   clobbered before `upsmon` reacts. The shared harness in M2 must
   call this out in an inline comment so it survives future edits.

3. **Test-only `SET` credential leaking into production.** The shared
   harness must make clear that `actions = [ "SET" ]` users exist only
   for tests. If a refactor accidentally consolidates the test credential
   with the production upsmon credential, production loses the minimal-
   privilege posture. Mitigation: the harness names them distinctly
   (`testops` vs. the `braid-ups-secrets.service`-managed upsmon user)
   and the Plan 1 module assertion already forbids `actions` on the
   production user.

4. **Forced-shutdown trigger coverage is UPS-only in this plan.** The
   framing is generic (kernel panic, OOM, hard power loss all produce
   the same post-reboot state) but the tests exercise only the UPS LB
   path because that is the trigger already wired by Plan 1. If a
   recover path has a subtle dependence on the specific shutdown
   sequence upsmon triggers (for example, a timing assumption that a
   non-UPS forced shutdown would violate), this plan's tests might
   green while real-world non-UPS forced shutdowns still break. The
   manual hardware smoke test above mitigates this by including a
   wall-cord-pull rehearsal. A future plan can formalize additional
   trigger harnesses (`poweroff -f`, kernel-panic injection) if
   that risk materializes.

5. **Runtime-budget overlap with Plan 1.** If Plan 1's M7 passed only
   with the M7b raised `battery.runtime.low`, the M3-M6 VM tests
   inherit that setting through the shared harness. If the harness
   accidentally drops the raised setting, the matrix tests may fail
   for runtime-budget reasons rather than recovery reasons.
   Mitigation: the harness either inherits Plan 1's module setting
   directly or sets its own known-large `battery.runtime.low`
   override with a comment pointing at ADR 020 Open Question 3.

## Pre-M11 audit findings (2026-04-21)

Verdict: **gap list, two concrete recover-side fixes, plus one minor follow-up
that the matrix tests will not exercise.** Both required fixes land in M1
before the VM matrix runs.

Per mutation class:

### Add (`cli/src/add.rs`)

- Bootstrap path (no mounted pool): the existing `NoBtrfs` special case in
  `cli/src/recover.rs:271-307` already produces the actionable
  "wipe-and-re-run" message. **No gap.**
- Existing-pool path: `pool_balance_raid1` (`cli/src/add.rs:580`) is the
  long phase. Forced shutdown leaves an in-flight balance whose convert
  filters the kernel persisted in the chunk tree's `BALANCE_ITEM`. braid
  always mounts with `skip_balance` (`cli/src/cmd.rs:271-283`), so the
  balance comes back **paused** on the next mount. `cmd_recover` only
  warns about it (`emit_paused_balance_warning`, `cli/src/recover.rs:421`),
  forcing the operator to run `btrfs balance resume` manually. The
  membership (`pool.json`) is correct, but RAID1 redundancy for the new
  data is not restored without manual action. **GAP A**.

### Remove (`cli/src/remove.rs`)

- `pool_remove_device` is the long phase. The kernel does not persist
  in-flight `device remove` state across reboots, so on next mount the
  removed-target device is still a member; live count == pre count.
  `cmd_recover` writes pool.json reflecting `pre_membership`, and
  `recovery_guidance` correctly tells the operator "remove did not
  complete -- re-run". **No gap.**
- 2->1 case only: `evict_present_device` runs `pool_balance_single`
  before the device remove (`cli/src/pool.rs:281-285`). Forced shutdown
  during that conversion leaves a paused single-conversion balance.
  Same shape as Add. **GAP A** (covered by the same fix).

### RemoveMissing (`cli/src/remove_missing.rs`)

- The post-removal `maybe_restore_raid1` soft balance
  (`cli/src/remove_missing.rs:219-225`, `cli/src/pool.rs:132-149`) is
  the long phase. Forced shutdown leaves a paused soft RAID1 balance.
  Same shape; same fix. **GAP A**.

### Replace (`cli/src/replace.rs`)

- `pool_replace_device` (`cli/src/replace.rs:318/350`) is the long
  phase. The kernel persists in-flight `dev_replace` state in the chunk
  tree and auto-resumes via `btrfs_resume_dev_replace_async` on the
  next mount. `cmd_recover` already handles this through
  `wait_for_kernel_replace_to_finish` and `relock_and_remount`
  (`cli/src/recover.rs:435-564`). **No gap for the replace itself.**
- Immediately after `pool_replace_device`, replace.rs runs
  `pool_resize_device` (`cli/src/replace.rs:327` Live and `:359`
  Missing) so the new disk reports its full capacity rather than the
  source disk's old upper bound. If shutdown lands between the
  kernel-resumed replace and `pool_resize_device`, the new disk's
  reported size is wrong and `cmd_recover` does **not** replay it.
  Recovered pool.json reads "completed" but the user has silently lost
  capacity. **GAP B**.
- Missing-path replace (only): the post-replace `maybe_restore_raid1`
  soft balance is the same shape as RemoveMissing. **GAP A**.
- Pre-LUKS-format interrupt (Replace targeting `PresentNotLuks`,
  shutdown lands after `write_journal` but before `luks_format`
  finishes): `plan_open_pool` will fail to open the new mapper because
  the device has no LUKS header yet. The current error path is opaque
  (recover surfaces a generic mount failure). The matrix tests target
  the slow phase, not this brief window, so this is **not load-bearing
  for ADR 020 sign-off**. Tracked as a follow-up; not remediated in
  this plan.

### Consolidated remediation

A single, narrowly-scoped change to `cmd_recover` closes both A and B:

1. Insert two steps **between** `save_membership` and `clear_journal`,
   in this exact order so any second forced shutdown is itself
   recoverable:

   - **Replace-only resize replay**: when `journal.op` is
     `OpKind::Replace { new_name, .. }`, look up `new_name`'s mapper in
     the just-probed live pool and call `pool_resize_device` (the
     command itself is idempotent, so this is safe even if the original
     resize already ran).
   - **Universal balance resume**: query `BalanceReport`; if `Paused`,
     call a new `pool_balance_resume` helper (which dispatches a new
     `CmdRequest::BtrfsBalanceResume` running `btrfs balance resume
     <mp>`). The kernel reuses the originally persisted convert filters,
     so a single resume covers Add (RAID1), 2->1 Remove (single +
     `mconvert=dup`), RemoveMissing (`raid1,soft`), and Missing-path
     Replace (`raid1,soft`).

2. Plumb `progress: ProgressOutput` through `RecoverParams` and
   `RecoverArgs` so the operator-facing resume shows progress while
   tests use `ProgressOutput::Off`.

No `OpKind` variant changes. No journal shape changes. No backward-compat
shims (per the no-backwards-compatibility constraint). The existing
`emit_paused_balance_warning` call after `clear_journal` becomes
unreachable in the recover path because the resume drains the paused
state; it is left in place because it is also called from `cmd_unlock`
(`cli/src/unlock.rs:101`), where the paused-balance is operator-set, not
braid-set.

### Why the resize/resume order matters

Order of operations in `cmd_recover` after the new logic:

1. `probe_pool` -> recovered membership.
2. `save_membership` -> pool.json reflects target.
3. **(NEW)** Replace-only `pool_resize_device`.
4. **(NEW)** Universal balance resume (slow; could itself be interrupted).
5. `clear_journal`.

If a second forced shutdown lands during step 4, the journal still
exists, pool.json already reflects target, and live state is
target-with-paused-balance. Re-running `braid recover` is idempotent:
probe sees target, save_membership rewrites the same content, resize is
idempotent, balance resume continues from where the second crash
stopped, then journal is cleared.

### M1 scope (closes Pre-M11)

Files touched, kept minimal:

- `cli/src/cmd.rs` -- add `CmdRequest::BtrfsBalanceResume { mount_point
  }` variant and its `to_argv` (= `btrfs balance resume <mp>`); add
  a unit test for the argv shape.
- `cli/src/pool.rs` -- add `pool_balance_resume` helper that wraps
  `BtrfsBalanceResume` through `run_with_progress` (`run_with_progress`
  already polls balance status in its loop, so it works for resume
  identically to the original balance start).
- `cli/src/recover.rs` -- insert the two new steps; new `progress`
  field on `RecoverParams`; two MockRunner-backed unit tests:
  - `recover_replays_resize_after_replace`
  - `recover_resumes_paused_balance_then_clears_journal`
- `cli/src/main.rs` -- pass `progress` from `RecoverArgs.common` (add a
  `CommonArgs`-style `--progress` flag scoped to `recover`).

The matrix tests (M3-M6) verify these paths end-to-end through forced
shutdown, kernel resume, and `braid recover` against real btrfs.

### Out of scope, captured as follow-up

- Pre-LUKS-format interrupt during `Replace` (Replace target gets
  partial / no LUKS header before crash). Matrix tests target the slow
  phase, not this fast window. Track as a future cleanup; ADR 020 sign-
  off does not depend on it.

## Cross-plan status dependency

**ADR 020 flips to `Active` in M7 of this plan, not earlier.** Plans 1
and 2 contribute to the ADR's guarantees but do not close the
recovery-proof gate. If a reviewer proposes flipping ADR 020 to
`Active` after Plans 1 and 2 land -- e.g., because "the user-facing
surface is done" -- push back: the ADR's "mid-mutation power loss is a
supported recovery case" sentence is load-bearing and is the whole
point of this plan.

If you think ADR 020 itself should be split into a v1-safety-core /
observability ADR (fillable from Plans 1+2) and a separate
recovery-proof ADR (this plan's scope), make that proposal explicitly
before acting on it. Splitting the ADR would let the safety-core and
observability work flip to `Active` sooner, at the cost of muddying
the contract that originally presented all three guarantees as one
promise. The current structure intentionally treats them as one
contract; changing that is a documentation-architecture decision, not
a scheduling shortcut.
