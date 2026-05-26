# Recovery-mode contract cleanup + scope `discover` refusals to `--write`

## Context

A `/verify-issue` pass started from a narrow `discover.md` docs mismatch
but, on review, the root cause is broader: the **recovery-mode contract**
("only `status`, `recover`, `lock` are permitted while `pending-op.json`
exists") is stated as an exhaustive whitelist in multiple authority
surfaces, and that whitelist is **factually wrong**.

### What the code actually does (verified this round)

`preflight::check_no_pending_operation` (`cli/src/preflight.rs:52`) is the
pending-op gate. It is **not** a global preflight before the `Commands`
match -- each command that needs it owns the call. The complete caller set:

- `add`, `remove`, `remove-missing`, `replace` -- `cli/src/main.rs:509,560,591,622`
- `unlock` -- `cli/src/unlock.rs:198`
- `enroll` -- `cli/src/enroll_key_file.rs:628`
- `discover --write` -- inline check in `write_discovered_membership`
  (`cli/src/discover.rs:587-592`)

Commands that do **not** gate on pending-op (verified absent):
`status`, `doctor` (`cli/src/doctor.rs` -- no call), `lock`, `recover`
(it consumes the journal), bare `discover`, and the monitoring/maintenance
surfaces (`ack`, `monitor`, `idle`, scrub commands, `ups`, `tui`).

So the true contract is class-based:

> While `pending-op.json` exists, the membership/mount/key-enrollment
> commands (`add`, `remove`, `remove-missing`, `replace`, `unlock`,
> `enroll`, `discover --write`) hard-fail; `braid recover` reconciles from
> live pool state and is the only command that clears the journal.
> Read-only diagnostic and cleanup surfaces (`status`, `doctor`, `lock`,
> bare `discover`) stay available.

The old "only status/recover/lock" whitelist misdescribes **`doctor`** and
**bare `discover`** as blocked when they are available. (Correction to the
prior plan round: my first grep searched only `main.rs` and wrongly
concluded `unlock`/`enroll` don't gate -- they do, inside their own
modules. The class-based contract above is the verified truth.)

### Separately: the discover lock dimension

`discover.md:72` claims bare `discover` refuses on pool-lock contention.
That is a different invariant (lock policy, not the journal):
`lock_policy` returns `None` for bare discover (`cli/src/main.rs:155-161`),
so it takes no lock -- correctly described in `docs/design/principles.md:69`
(Principle 12), `docs/design/decisions/018-systemd-lifecycle.md:152`, and
pinned by `tests/module/pool-lock-discover-contention.py:54-58`. This is
also `--write`-only and needs scoping.

**Outcome:** fix the recovery-mode contract at its source (4 copies), fix
the two operator-doc surfaces that present discover's pending-op/lock
refusals as unconditional, and add the one missing behavioral test.

## Edits

### Layer 1 -- Recovery-mode contract (root cause)

Replace the exhaustive "only `status`, `recover`, `lock`" whitelist with
the class-based contract above. Same logical shape in all four copies,
adapted to each site's register (terse for doc comments, fuller for ADR):

| File:line | Current (wrong) |
| --- | --- |
| `cli/src/preflight.rs:50-51` | doc comment: "only `status`, `recover`, and `lock` are safe to run" (also fix the stray `—` em-dash to `--`) |
| `cli/src/journal.rs:13-14` | doc comment: "only `status`, `recover`, and `lock` are permitted. All other commands hard-fail." |
| `docs/design/principles.md:22` | Principle 3: "recovery mode -- only `status`, `recover`, `lock` are permitted." |
| `docs/design/decisions/017-runtime-disk-membership.md:65` | ADR: "All commands except `status`, `recover`, and `lock` hard-fail." |

For each, state the blocked class (the 7 membership/mount/key-enrollment
commands), that `recover` is the sole journal-clearing path, and that
read-only diagnostic/cleanup surfaces (`status`, `doctor`, `lock`, bare
`discover`) stay available. The `preflight.rs` comment should additionally
name itself as the gate those 7 commands call.

### Layer 2 -- Operator docs (discover-specific symptoms)

#### `docs/commands/discover.md`

| Line | Now (wrong) | Target |
| --- | --- | --- |
| 55 (under-the-hood step 1) | "Checks for a pending operation journal (refuses if one exists)." | "With `--write`, refuses if a pending operation journal (`pending-op.json`) exists. Bare `discover` is read-only and skips this gate." |
| 71 (Safety checks, pending-op) | "Refuses if a pending operation journal ... exists -- run `braid recover` ..." | Prefix with "With `--write`, refuses if ..." (rest unchanged). |
| 72 (Safety checks, pool lock) | "Refuses if another braid operation is in progress (pool lock ...) ..." | Prefix with "With `--write`, refuses if ..." (rest unchanged). |
| 74 (Safety checks) | "Without `--write`, makes no changes at all -- read-only scan." | Strengthen: "... read-only scan that takes no pool lock and does not consult the pending-op journal." |

Line 74 carries the single positive statement of bare discover's
gate-free behavior (keeps the scoping DRY vs. a parenthetical per line).
Keep step 1 in place (no renumber) -- the under-the-hood list is a logical
sequence and step 2's `pool.json` gate is already a top-of-list
precondition.

#### `docs/guides/recovery-scenarios.md:75`

| Now (wrong) | Target |
| --- | --- |
| "`discover` refuses to run if pending-op.json exists. Use `braid recover` instead." | "`discover --write` refuses to run if `pending-op.json` exists. Use `braid recover` instead. (Bare `discover` is read-only and runs regardless.)" |

This note sits inside a `discover --write` rebuild recipe (lines 60-69), so
`--write` scoping is correct and contextually natural.

### Layer 3 -- Behavioral coverage for the contract's named surfaces

The class-based contract makes explicit, testable claims about which
commands stay available while `pending-op.json` exists. Pin the two named
available surfaces that lack coverage. (`status` is already covered by
`cli/src/status.rs::build_status_surfaces_pending_op_advisory_when_mounted`;
the blocked membership/mount/key-enrollment set is covered by existing
refusal tests -- `discover.rs:1785` for `discover --write`, the `unlock`/
`enroll` recovery-mode tests, and `mutating-config-preflight-order.py`.)

**3a. Bare `discover` is available (closes the original gap).** Existing
coverage does not prove bare discover ignores a pending journal:
`pool-lock-discover-contention.py:54-58` pins only lock contention, and
`discover.rs:1785` pins `discover --write` refusing pending-op. Add a
subtest to `tests/cli/braid-discover.py`, inserted after the "discover
without --write does not create pool.json" subtest (line 98-99) where
`pool.json` is still absent -- the recovery-diagnostic scenario:

- Seed `/var/lib/braid/pending-op.json` with a valid-looking journal (bare
  discover never reads it, so existence is the point; valid JSON keeps the
  fixture realistic).
- Run bare `braid discover`; assert rc 0, the `disk1`/`disk2` preview and
  the `pass --write to save` hint are present, and NO pending-op refusal
  appears (`"interrupted operation"` / `"pending-op.json"` absent).
- `rm /var/lib/braid/pending-op.json` afterward so later subtests run clean.

**3b. `doctor` and `lock` are available.** `tests/cli/braid-recover.py`
already locks the pool and injects a journal (Phase 2, ending line 81), so
reuse that state: insert a subtest immediately after line 81 (before the
"unlock refuses" subtest at line 85), while the pool is locked and
`pending-op.json` is present:

- Run `braid doctor --json`; assert it reaches its normal diagnostic report
  (parseable JSON) rather than hard-failing at a gate, and that the gate
  refusal text (`"interrupted operation detected (pending-op.json exists"`)
  is absent. Use `machine.execute` and do **not** assert rc 0 -- doctor
  legitimately flags the pending op as a finding, so it may exit non-zero
  while still being "available".
- Run `braid lock`; assert it completes its normal teardown path (pool
  already locked -> idempotent no-op outcome) without the gate refusal.
  This is the achievable scenario: a mounted pool with a pending journal
  is not reachable here because `unlock` refuses during pending-op.
- The inserted subtest changes no state (doctor read-only; lock-while-locked
  is a no-op), so the existing Phase 3 "unlock refuses" subtest is unaffected.

Both new subtests carry the three-section preamble (Intent / Why it exists /
Scenario) per `docs/dev/testing.md` and the surrounding subtest style.

## Out of scope / non-goals

- **No production code-path changes.** The gating behavior is correct as
  built; Layer 1 only rewrites doc comments + design prose, Layer 2 is
  operator docs, Layer 3 adds tests. `check_no_pending_operation` and its
  callers are unchanged.
- **No restructuring** of discover.md's under-the-hood numbered list beyond
  scoping step 1.
- **Monitoring/maintenance commands** (`ack`, `monitor`, `idle`, scrub
  commands, `ups`, `tui`) also do not gate on pending-op, but the contract
  names them only illustratively ("such as"), not as a guarantee. Layer 3
  pins exactly the four explicitly-named available surfaces (`status`,
  `doctor`, `lock`, bare `discover`); separate availability tests for the
  monitoring commands would over-test beyond the plan's stated claims.
- **`docs/commands/enroll.md`** mentions "recovery mode" but does not repeat
  the exhaustive whitelist (it describes enroll's own -- correct -- blocking).
  Verify during implementation that it stays consistent with the new
  contract wording; edit only if it contradicts.

## Verification

- `just test-vm braid-recover braid-discover` -- exercises the new
  `doctor`/`lock` availability subtest (braid-recover, Layer 3b) and the
  bare-discover pending-op subtest (braid-discover, Layer 3a), plus both
  existing suites, on a live VM.
- `mdbook build docs` from repo root -- validates the docs tree and
  cross-links (`mdbook-linkcheck` per AGENTS.md).
- Re-read all four Layer-1 contract copies side by side and confirm one
  consistent class-based shape; cross-check the named blocked/available
  sets against the verified caller list (`add`, `remove`, `remove-missing`,
  `replace`, `unlock`, `enroll`, `discover --write` block; `status`,
  `doctor`, `lock`, `recover`, bare `discover` do not).
- Cross-check the corrected operator docs against their behavioral pins:
  `tests/module/pool-lock-discover-contention.py:54-58`,
  `docs/design/principles.md:69`, `docs/design/decisions/018-systemd-lifecycle.md:152`.
- Layer 1 changes are comment/prose only (no behavior), so no Rust test run
  is required for them; the new VM test is the behavioral pin for Layer 2/3.
