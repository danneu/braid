# braid improvement hunt -- full report (2026-08-05)

Multi-agent audit of the repo at commit `9aa31021` (master, clean tree).

## Launch a verification tab

From a fish shell inside the DanTerm tab group where the work should appear,
set a task-specific title and `$verify-issue` prompt, then launch Codex at medium
effort in a new background tab:

```fish
set group_id (danterm pane info --pane $DANTERM_PANE | jq -r '.group.id')
set tab_title 'TASK-1 + TASK-2 - CLI UX'
set verify_prompt '$verify-issue TASK-1 + TASK-2 - CLI UX: Verify the monitor exit-code and replace --new help findings. Investigate current code, history, sibling behavior, and the right scope; recommend work only and do not implement. Audit: [braid improvement hunt, TASK-1 and TASK-2](/Users/dan/Code/braid/scratch/2026-08-05-improvement-hunt.md)'
set escaped_prompt (string escape -- $verify_prompt)

danterm tab new --group $group_id \
    --cwd /Users/dan/Code/braid \
    --title $tab_title \
    --cmd "codex -c model_reasoning_effort=medium $escaped_prompt"
```

For another lane, replace the title and prompt with its task name(s), category,
short description, and a link to the corresponding task section in this audit.
Keep tasks that share critical files in the same tab.

When an agent implements and commits a verified task, it must update that task's
tracker row in this file in the same commit: set **Status** to `Done` and
**Completed** to the commit date in `YYYY-MM-DD` form. Verification-only sessions
leave the tracker unchanged.

Do not mention `TASK-N` identifiers or this audit document in commit messages,
code comments, or documentation comments. Describe the change and its rationale
in domain terms; the tracker-row update is the only task-number bookkeeping.

**Run:** 47 Opus 5 subagents -- 7 lens-scoped finders (dead/duplicated logic,
over-complex control flow, comment/doc drift, API and naming altitude, error
handling, algorithmic/allocation waste, test gaps), 1 dedup pass, and one
adversarial verifier per candidate instructed to refute the claim against the
actual source (default: reject when uncertain). ~3.2M subagent tokens, 1275
tool calls, 28 minutes.

**Outcome:** 40 raw candidates -> 39 after dedup -> **37 confirmed**,
**2 refuted**. No finding rated high-value: what survived is real but
modest -- help-text/doc drift, genuine duplication, dead scaffolding, and
test-lane gaps. Findings are ranked by the verifier's independent value
rating, then effort. `value/effort` on each entry is that rating.

**Where to start:** TASK-1 through TASK-6 are one-line fixes with operator-facing payoff;
TASK-7, TASK-11, TASK-13, and TASK-16 delete genuinely dead weight the compiler will then
police; TASK-18 closes the most surprising test-lane hole (the authoritative
golden lane being the lenient one).

## Task tracker

Set **Status** to `Done` and **Completed** to an ISO date (`YYYY-MM-DD`) when a
task lands. `Rejected` tasks are retained for reference and are not pending work.

| Task | Status | Completed | Categories | Proposal |
| --- | --- | --- | --- | --- |
| TASK-1 | Done | 2026-08-24 | Documentation, CLI UX | Document monitor exit 3 for Warning-only alerts in clap help. |
| TASK-2 | Done | 2026-08-24 | Documentation, CLI UX | Describe replace `--new` as a full `NAME=/dev/disk/by-id/...` disk spec. |
| TASK-3 | Done | 2026-08-24 | Documentation, testing | Classify the cryptsetup LUKS dump parser as CLI-reachable and accurately describe its canary coverage. |
| TASK-4 | Done | 2026-08-24 | Simplification, API | Delete three unused `UpscOutput` predicates superseded by `severity()`. |
| TASK-5 | Done | 2026-08-24 | Error handling, observability | Preserve and print `systemctl_start` failure details in the online-state warning. |
| TASK-6 | Done | 2026-08-24 | Testing | Add the omitted `root_check` and `tty_passphrase` integration targets to `test-rust`. |
| TASK-7 | Done | 2026-08-24 | Simplification, dead code | Remove the obsolete TUI-wide dead-code allowance and let the compiler expose unused code. |
| TASK-8 | Done | 2026-08-24 | Simplification, duplication | Extract the duplicated scoped `btrfs device scan --forget` warning-and-continue block. |
| TASK-9 | Done | 2026-08-24 | Simplification, error modeling | Centralize mapper-ownership messages shared by `LuksError` and `ProbeError`. |
| TASK-10 | Done | 2026-08-24 | Simplification, testing | Centralize the fixture-path helper copied across 13 parser modules. |
| TASK-11 | Done | 2026-08-24 | Simplification, dead code | Remove six unused fixture re-exports and their suppressing allowances. |
| TASK-12 | Done | 2026-08-24 | Simplification, dry-run | Share LUKS enrollment and header-backup preview steps across add, replace, and enroll. |
| TASK-13 | Done | 2026-08-24 | Simplification, data modeling | Delete the uninhabited partial-preview scaffolding and stale completeness claims. |
| TASK-14 | Done | 2026-08-24 | Data modeling, architecture | Move the crate-wide `Filesystem` seam out of the probe-specific module. |
| TASK-15 | Done | 2026-08-24 | Error handling, recovery UX | Give remove-missing post-commit failures dedicated recovery-aware error variants. |
| TASK-16 | Done | 2026-08-24 | Testing | Make the missing NVMe healthy fixture test fail instead of silently skipping. |
| TASK-17 | Done | 2026-08-24 | Testing, error handling | Lock the live btrfs minimum-devices stderr contract used for recovery hints. |
| TASK-18 | Done | 2026-08-24 | Testing, reliability | Make the authoritative stable golden lane fail when required fixtures are missing. |
| TASK-19 | Done | 2026-08-24 | Testing, error handling | Add a live-tool lock for the `btrfs balance pause` exit-code and stderr classifier. |
| TASK-20 | Done | 2026-08-24 | Testing, tooling | Add a self-test proving the decision-document path checker can fail. |
| TASK-21 | Done | 2026-08-24 | Performance, simplification | Replace three per-disk `lsblk` calls with one parsed JSON invocation. |
| TASK-22 | Done | 2026-08-24 | Simplification, testing | Use `PanicBtrfsDevInfo` to enforce every no-probe replace boundary and remove its allowance. |
| TASK-23 | Done | 2026-08-24 | Simplification, configuration | Remove the redundant positive-timeout assertion already enforced by the Nix type. |
| TASK-24 | Done | 2026-08-24 | Simplification, duplication | Construct the replace RAID1 soft-balance preview step in one place. |
| TASK-25 | Done | 2026-08-24 | Simplification, duplication | Share the open-step literal and vary only the keyfile/passphrase command. |
| TASK-26 | Done | 2026-08-24 | Simplification, control flow | Merge consecutive normal-mode branches in TUI command completion. |
| TASK-27 | Done | 2026-08-24 | Documentation, configuration | Correct ADR 020's NUT package-option name. |
| TASK-28 | Done | 2026-08-24 | Documentation, CLI UX | Make add `--enroll` help clear that every adopted disk is enrolled. |
| TASK-29 | Done | 2026-08-24 | Simplification, testing | Remove duplicated probe-event rendering and test the production rendering path. |
| TASK-30 | Done | 2026-08-24 | Developer tooling | Make `clippy-fix` actually apply clippy suggestions and align its scope with `clippy`. |
| TASK-31 | Done | 2026-08-24 | Simplification, API | Remove the unreachable dry-run boolean from the lock orchestrator callback seam. |
| TASK-32 | Done | 2026-08-24 | Error handling, observability | Print `PoolLockError` so lock-file failures retain their braid-layer context. |
| TASK-33 | Done | 2026-08-24 | Error handling, consistency | Tag recover probe failures consistently with other command-level errors. |
| TASK-34 | Done | 2026-08-24 | Tooling, documentation | Track ASCII-guard allowances on the clap doc line that owns each buffered hit. |
| TASK-35 | Done | 2026-08-24 | Simplification, control flow | Compute repeated recovery-plan predicates once and reuse them. |
| TASK-36 | Done | 2026-08-24 | Documentation | Remove stale parser-test citations and document tolerant parsing inline. |
| TASK-37 | Done | 2026-08-24 | Simplification, API | Make always-successful online-state helpers return `()` instead of `Result`. |
| TASK-38 | Rejected | -- | Simplification, control flow | Merge duplicate-looking unmount terminal branches; rejected because their separation is deliberate. |
| TASK-39 | Rejected | -- | Testing, data modeling | Share grammar samples across Nix and Rust; rejected because the claimed parity failure is not real. |

## Index

**Medium value, trivial effort**

1. **TASK-1:** `cli/src/main.rs:78` -- The `braid monitor` clap help enumerates exit codes 0/1/2 but omits exit 3, the Warning-tier (ENOSPC risk, non-beeping) exit that the dispatcher actually returns.
2. **TASK-2:** `cli/src/main.rs:358` -- `braid replace --new` is documented in `--help` as taking a disk name, but the code parses it as a full `NAME=/dev/disk/by-id/...` disk spec and errors on a bare name.
3. **TASK-3:** `docs/dev/parser-compatibility.md:13` -- parser-compatibility.md still classifies `parse_cryptsetup_luks_dump` as a TUI-only parser, but it is on the `braid replace` pre-journal safety path that sizes the replacement target.
4. **TASK-4:** `cli/src/parse/types.rs:771` -- Three `pub fn` predicates on `UpscOutput` (`is_critical`, `is_on_battery`, `reports_utility_power`) have zero call sites anywhere in the repo, and their doc comments assert caller relationships that do not exist -- the live UPS safety path routes through `UpscOutput::severity()` instead.
5. **TASK-5:** `cli/src/online_state.rs:298` -- `mark_online` discards the `OnlineError` from `systemctl_start` via `.is_err()`, so the warning omits the exit code and stderr that the error variant already carries -- the only `.is_err()`-style error swallow left in the CLI.
6. **TASK-6:** `justfile:121` -- `just test-rust` selects integration-test targets explicitly and omits `root_check` and `tty_passphrase`, so 10 real integration tests have never run in the Rust lane or in CI.
**Medium value, small effort**

7. **TASK-7:** `cli/src/lib.rs:66` -- The blanket `#[allow(dead_code)]` over the whole `tui` module is justified by a comment claiming the TUI is "stubbed out", which has been false for months -- it now suppresses dead-code detection across the crate's largest subtree.
8. **TASK-8:** `cli/src/mount.rs:703` -- The "scope `btrfs device scan --forget` to existing close-set paths, warn and continue on failure" block is implemented twice, byte-for-byte identical including both operator-facing warn strings.
9. **TASK-9:** `cli/src/luks.rs:116` -- Three user-facing mapper-ownership error messages are duplicated verbatim between `LuksError` and `ProbeError`, despite the codebase already centralizing one sub-fragment of them precisely to stop this pair from drifting.
10. **TASK-10:** `cli/src/parse/btrfs_device_usage.rs:162` -- The same six-line `fn fixture(name: &str) -> String` test helper is copy-pasted byte-for-byte into 13 parser modules, all resolving the identical `tests/fixtures/nixos-26.05/` path.
11. **TASK-11:** `cli/src/test_fixtures.rs:165` -- Six fixture re-exports in `test_fixtures.rs` are used by no test module -- they are consumed only inside their own defining fixture file -- and the `#[allow(unused_imports)]` on those blocks is what keeps the compiler from saying so.
12. **TASK-12:** `cli/src/replace.rs:312` -- `ReplaceWorkPlan::render_steps`'s `ExistingLuks` arm re-implements, byte-for-byte, the two preview steps that `add.rs` already factored into the `push_returned_disk_enrollment_steps` helper, so the two commands' dry-run wording can silently drift.
13. **TASK-13:** `cli/src/preview.rs:9` -- The `PreviewCompleteness`/`PreviewGap` scaffolding never materialized: `Partial` is constructed only in one test, `PreviewGap` is still uninhabited, the render branch is an empty loop with commented-out code, and the three `PR 0` doc claims are now false since every mutating command has migrated to `Preview`.
14. **TASK-14:** `cli/src/probe.rs:17` -- The crate-wide `Filesystem` seam lives in `probe.rs` under a banner that describes only `Path::exists()`, so unrelated modules must import `crate::probe::Filesystem` to do filesystem work that has nothing to do with probing.
15. **TASK-15:** `cli/src/remove_missing.rs:249` -- `remove-missing`'s post-commit failures (pool.json persist, journal rewrite, journal clear) all collapse into the generic `Validation` variant with no recovery remediation, unlike the structurally identical `remove`, which has dedicated variants that tell the operator to run `braid recover`.
16. **TASK-16:** `cli/src/parse/smartctl.rs:412` -- `nvme_fixture_healthy` reads a fixture that does not exist and can never be captured, so it silently returns without asserting anything on every run.
17. **TASK-17:** `tests/repro/remove-without-balance.py:56` -- The `stderr.contains("unable to go below")` classifier that produces braid's min-devices recovery hint has no live-tool behavior lock; the repro that reproduces the exact scenario discards btrfs's stderr.
18. **TASK-18:** `cli/tests/golden_nixos_26_05.rs:11` -- The authoritative stable golden lane skips silently on a missing fixture while the explicitly non-authoritative unstable lane hard-fails, inverting the intended strictness.
19. **TASK-19:** `cli/src/lock.rs:684` -- The `exit_status == 2 && stderr.contains("Not running")` classifier for `btrfs balance pause` has no live-tool test asserting that code and wording.
20. **TASK-20:** `justfile:348` -- `check-see-paths.py` is the only docs guard with no `--selftest`, so nothing proves it can still fail; a regex or path-resolution regression would make it pass vacuously.
**Medium value, medium effort**

21. **TASK-21:** `cli/src/confirm.rs:147` -- `query_disk_hw_info` spawns three separate `lsblk` processes per disk to read MODEL, SERIAL and SIZE, even though the repo already has a fixture-covered `lsblk --json` parser that returns all three fields from one invocation.
**Low value, trivial effort**

22. **TASK-22:** `cli/src/btrfs_ioctl.rs:117` -- `PanicBtrfsDevInfo` is test-support scaffolding with zero call sites anywhere in the repo; its `#[allow(dead_code)]` exists solely to silence the warning that would otherwise report it.
23. **TASK-23:** `modules/braid/options.nix:119` -- The `braid.autoUnlock.timeoutSec > 0` assertion can never fire because the option's type is already `lib.types.ints.positive`, which rejects any non-positive value during option type-checking.
24. **TASK-24:** `cli/src/replace.rs:390` -- The RAID1 soft-balance `Step` (risk tag, dry-run description, and command) is constructed twice with identical text, even though the decision to emit it is already single-sourced through `pool::should_restore_raid1`.
25. **TASK-25:** `cli/src/mount.rs:359` -- `compile_open_steps` duplicates the whole `Step` literal across the keyfile/passphrase branches when only the single `CmdRequest` inside `commands` differs.
26. **TASK-26:** `cli/src/tui/browse/state.rs:728` -- `command_finished` opens two consecutive `if self.mode == BrowseMode::Normal` blocks with no mutation of `self.mode` between them.
27. **TASK-27:** `docs/design/decisions/020-ups-integration.md:55` -- Active ADR 020 names the NUT pin option `braid.packages.networkupstools`, but the module implements it as `braid.packages.nut`.
28. **TASK-28:** `cli/src/main.rs:324` -- `braid add --enroll`'s help says the keyfile is enrolled in "the new disk" (singular), but `add` accepts multiple disk specs and enrolls slot 1 on every adopted disk.
29. **TASK-29:** `cli/src/mount.rs:332` -- `print_probe_events`'s doc claims it is a thin wrapper around `render_probe_events`, but it does not call it -- it duplicates the body with a different color argument, so the two produce different bytes, and the "byte-for-byte stable" test pins the twin no production path uses.
30. **TASK-30:** `justfile:139` -- The `clippy-fix` recipe never runs clippy -- it runs `cargo fix`, which applies only rustc's machine-applicable suggestions, so `just clippy-fix` cannot fix anything `just clippy` reports.
31. **TASK-31:** `cli/src/lock.rs:1180` -- `cmd_lock_orchestrate_impl`'s `CL` closure bound carries a `bool` dry-run parameter that the function itself hardcodes to `false`, so the seam advertises a dry-run mode the orchestrator can never enter and every test closure has to name and ignore it.
32. **TASK-32:** `cli/src/main.rs:1198` -- `handle_pool_lock_error` prints the inner `io::Error` instead of the `PoolLockError`, discarding the variant's own `pool lock I/O error:` context, so a lock-file failure surfaces as a bare, unattributable errno.
33. **TASK-33:** `cli/src/recover.rs:34` -- `RecoverError::Probe` is an untagged `#[error("{0}")]` passthrough while every other command-level enum tags the same `ProbeError` wrapper with `probe error:`, so a probe failure during `braid recover` prints with no indication of which braid layer produced it.
**Low value, small effort**

34. **TASK-34:** `scripts/docs/check-output-ascii.py:189` -- The buffered clap-doc flush consults `self.line_allow` of the `#[derive(...)]` line rather than of the doc line the hit came from, so the `// ascii-guard: allow` escape hatch is both ignored on `///` clap-help lines and over-broad on the derive line.
35. **TASK-35:** `cli/src/recover.rs:1419` -- `plan_recover` re-tests `open_plan.is_some()` three times and `is_replace_pool_mutation(&journal.op)` twice across three consecutive blocks that all key off the same two facts.
36. **TASK-36:** `cli/src/parse/btrfs_device_stats.rs:134` -- A test doc comment cites `cli/docs/command-capabilities.md`, a file deleted from the repo in April 2026.
37. **TASK-37:** `cli/src/online_state.rs:265` -- `mark_online` and `mark_offline` are typed `Result<(), OnlineError>` but return `Ok(())` on every path (all failures are warned in place), so both call sites need a `let _ =` that reads like a swallowed error when nothing is actually being discarded.

**Rejected on verification**

- **TASK-38:** `cli/src/lock.rs:462` (control-flow) -- `umount_with_retry` has two adjacent branches that build and return the identical `build_umount_error(...)` value, duplicating the terminal-failure path.
- **TASK-39:** `tests/eval/grammar-parity.nix:1` (test-gaps) -- The grammar-parity check's stated intent is Nix/Rust parity, but it only evaluates the Nix predicates against a Nix-local sample list -- the Rust side is never invoked, so a one-sided grammar change passes.

## Confirmed findings

### Medium value, trivial effort

#### TASK-1: `cli/src/main.rs:78` [medium/trivial]

*Lens: doc-drift | category: doc-drift*

**Claim:** The `braid monitor` clap help enumerates exit codes 0/1/2 but omits exit 3, the Warning-tier (ENOSPC risk, non-beeping) exit that the dispatcher actually returns.

**Before:**

~~~~
    /// Check disk health: exit 0 = ok/offline/lock-contended, exit 1 = alert (incl. probe/compute failure latched as ComputationError), exit 2 = setup error (e.g. pool-lock I/O, config load)
    Monitor,
~~~~

**After:**

~~~~
    /// Check disk health: exit 0 = ok/offline/lock-contended, exit 1 = Critical alert (incl. probe/compute failure latched as ComputationError), exit 2 = setup error (e.g. pool-lock I/O, config load), exit 3 = Warning-only alert (ENOSPC risk; no beep)
    Monitor,
~~~~

**Finder's evidence:** cli/src/main.rs#main dispatch for `Commands::Monitor` (lines 934-945) matches on `state.severity()` and does `Some(braid_cli::alert::AlertSeverity::Warning) => std::process::exit(3)`. docs/commands/monitor.md documents the tier explicitly at line 49 ("**Warning (exit 3, no beep ...)**") and line 73 ("--exit 3--> braid-alert-advisory.service"), and docs/commands/status.md:379 says "`braid monitor` Warning (exit 3, no beep)". Only the `--help` text is stale, so the one surface an operator reads at the terminal is the one that is wrong.

**Verifier's correction:** Core claim holds as stated. Minor precision: exit 1 also catches the `None`-severity fail-closed fallback (an Alert with no causes), which the proposed "exit 1 = Critical alert" wording subsumes acceptably given the adjacent in-code comment already explains it.

**Verifier's reasoning:** Read cli/src/main.rs:78 — the "before" text matches verbatim. The dispatch arm at lines 934-945 matches on state.severity() and does `Some(AlertSeverity::Warning) => std::process::exit(3)`, and cli/src/alert.rs#AlertCause::severity makes EnospcRisk the sole Warning cause, so exit 3 is genuinely reachable and undocumented in --help. ADR 018 (docs/design/decisions/018-systemd-lifecycle.md) carries the "canonical exit-code table" listing exit 3 = Warning-only alert -> braid-alert-advisory.service, no beep; ADR 014 and docs/commands/{monitor,status}.md agree, so the help string is the only stale surface. `git log -L 78,79:cli/src/main.rs` shows the line was deliberately kept as a precise exit-code summary across four commits, the last (e3e22be8, 2026-05-28) predating the tier commit cf28ce7f (2026-06-19) — an oversight, not policy. Fix is ASCII-only, no test or parity script asserts help text, and cmd_monitor's own doc comment enumerates no exit codes, so nothing else needs syncing.

#### TASK-2: `cli/src/main.rs:358` [medium/trivial]

*Lens: doc-drift | category: doc-drift*

**Claim:** `braid replace --new` is documented in `--help` as taking a disk name, but the code parses it as a full `NAME=/dev/disk/by-id/...` disk spec and errors on a bare name.

**Before:**

~~~~
    /// Disk name of the new replacement disk
    #[arg(long, add = ArgValueCandidates::new(disk_name_candidates))]
    new: String,
~~~~

**After:**

~~~~
    /// Disk spec for the replacement disk: NAME=/dev/disk/by-id/... (e.g. wd1=/dev/disk/by-id/ata-WDC_WD120EFBX-68B0EN0_ZZZZ)
    #[arg(long, add = ArgValueCandidates::new(disk_name_candidates))]
    new: String,
~~~~

**Finder's evidence:** cli/src/replace.rs line 1244 does `membership::parse_disk_spec(params.new_name)` and returns `ReplaceError::Validation` on failure. Every other surface already says spec: docs/commands/replace.md:74 `| --new <name>=<path> |`, README.md:151 `sudo braid replace --old toshiba2 --new wd1=/dev/disk/by-id/...`, and `cli/src/repair_hint.rs` emits `braid replace --old <missing-name> --new <new-name>=/dev/disk/by-id/<...>`. The sibling `AddArgs.disks` help (main.rs:321) uses the correct "Disk specs: NAME=/dev/disk/by-id/..." wording, and `--old` (main.rs:355) genuinely is a bare name -- so the two flags read identically in `--help` while behaving differently.

**Verifier's correction:** Core claim stands as written. Two refinements: (a) the failure is a clean fail-closed refusal with a self-explanatory message ("expected NAME=/dev/disk/by-id/..., got '<x>'"), not a silent misbehavior, so this is friction/inconsistency rather than a correctness hazard; (b) while editing, also set `value_name = "NAME=BY_ID"` on the `#[arg(...)]` so the usage line reads `--new <NAME=BY_ID>` instead of `--new <NEW>` -- the help synopsis is the part a user scans first. Separately (out of scope, do not fold in): `--new` carries `add = ArgValueCandidates::new(disk_name_candidates)`, which completes *existing* pool disk names for a flag that must name a *new* disk; that is a distinct question worth its own look.

**Verifier's reasoning:** The "before" text is verbatim at `/Users/dan/Code/braid/cli/src/main.rs:358` (`ReplaceArgs::new`), and I confirmed the drift end-to-end: `main.rs` passes `new_name: &args.new` (line 686) into `replace.rs:1244`, which calls `membership::parse_disk_spec(params.new_name)` and returns `ReplaceError::Validation` on failure; `membership.rs#parse_disk_spec` hard-requires a `=` (`DiskSpecParseError::Shape` -- covered by the unit test `parse_disk_spec_shape_error`, which asserts a bare `"toshiba"` fails), while `--old` goes through plain `DiskName::parse` (replace.rs:1313), so the two identically-worded flags genuinely behave differently. I rendered `braid replace --help` and it prints `--new <NEW>  Disk name of the new replacement disk` -- contradicting every other surface (`docs/commands/replace.md:74` `--new <name>=<path>`, README.md:151, `cli/src/repair_hint.rs:95`) and the sibling `AddArgs::disks` help at main.rs:321 which correctly says "Disk specs: NAME=/dev/disk/by-id/...". I grepped for help-output snapshots/trycmd fixtures and found none outside `plans/`, so the one-line doc-comment edit breaks no test; the proposed wording is ASCII-only and matches the sibling flag's house style, and no principle or ADR designs for the terse wording.

#### TASK-3: `docs/dev/parser-compatibility.md:13` [medium/trivial]

*Lens: doc-drift | category: doc-drift*

**Claim:** parser-compatibility.md still classifies `parse_cryptsetup_luks_dump` as a TUI-only parser, but it is on the `braid replace` pre-journal safety path that sizes the replacement target.

**Before:**

~~~~
- Fixture refresh is a separate obligation: `just test-parsers` passing does not guarantee TUI-only parsers (`parse_lsblk_json`, `parse_cryptsetup_luks_dump`) or unused parsers (`parse_btrfs_scrub_status_per_device`) are compatible with the current toolchain.
~~~~

**After:**

~~~~
- Fixture refresh is a separate obligation: `just test-parsers` passing does not guarantee the TUI-only parser (`parse_lsblk_json`), the CLI-reachable-but-uncanaried `parse_cryptsetup_luks_dump` (used by `braid replace`'s target-capacity preflight, which no `test-parsers` lane exercises), or unused parsers (`parse_btrfs_scrub_status_per_device`) are compatible with the current toolchain.
~~~~

**Finder's evidence:** `cli/src/preflight.rs#check_replace_target_capacity` calls `parse_cryptsetup_luks_dump(&raw)` on `CmdRequest::CryptsetupLuksDump` to derive LUKS2 segment offset/size, and it is invoked from `cli/src/replace.rs` line 1416. docs/internals/luks-unlock.md#replace-target-size-preflight documents the same path ("Existing LUKS targets read LUKS2 segment `offset` and `size` from `cryptsetup luksDump --dump-json-metadata`"), and docs/commands/replace.md:116 documents the resulting refusal. The doc's very next bullet (line 14) already performs exactly this correction for `parse_smartctl` ("so it is no longer TUI-only"), so the classification simply was not carried over. `just test-parsers` (justfile) runs braid-status-rust/-during-balance/-ups, braid-idle, braid-discover, braid-tui-browse -- none drive `replace` -- so the "not guaranteed" half stays true; only the "TUI-only" label is now false, and it understates the blast radius of a cryptsetup bump on a destructive command.

**Verifier's correction:** The core claim (the "TUI-only" classification is false; it is on `braid replace`'s pre-journal capacity preflight) holds, but the proposed "after" overstates the gap by calling it "uncanaried": the parser IS exercised against live cryptsetup output by registered VM checks -- `tests/cli/replace-new-already-luks.py` and `tests/cli/replace-enroll-existing-luks.py` pre-`luksFormat` the target and then run `braid replace`, hitting the `PresentLuks` arm, and both are in `flake.nix` checks (`just test-all`). It is only absent from the `just test-parsers` recipe (braid-status-rust/-during-balance/-ups, braid-idle, braid-discover, braid-tui-browse). Accurate rewrite: move it out of the TUI-only list and say it is CLI-reachable via `braid replace`'s target-capacity preflight, covered live by the replace VM tests but not by the `just test-parsers` canary lane. Worth folding in the same stale header at `cli/tests/support/golden_common.rs` ("--- TUI-only parsers (not exercised by CLI commands in VM tests) ---" above `golden_cryptsetup_luks_dump`).

**Verifier's reasoning:** Verified the "before" text verbatim at docs/dev/parser-compatibility.md line 13. `rg` shows `parse_cryptsetup_luks_dump` has exactly two production call sites: `cli/src/tui/probe.rs` and `cli/src/preflight.rs#check_replace_target_capacity` (line 507), which `cli/src/replace.rs:1416` invokes during replace planning before the journal write -- so the "TUI-only" label is factually false today (git: `8452c550 fix(replace): preflight target mapper capacity`, 2026-05-23, predates the doc's last touch 0de4513c on 2026-06-24), and docs/internals/luks-unlock.md#replace-target-size-preflight documents the same path. The sibling `parse_lsblk_json` claim in the same bullet is still correct (only `tui/probe.rs` parses lsblk JSON; `confirm::query_disk_hw_info` uses plain `lsblk -no` fields), and line 14 already applied this same correction for `parse_smartctl`, so the fix is in-house style, not taste. Nothing in principles.md or an Active ADR makes the old wording deliberate; the edit is doc-only and breaks no test.

#### TASK-4: `cli/src/parse/types.rs:771` [medium/trivial]

*Lens: api-naming | category: dead-code*

**Claim:** Three `pub fn` predicates on `UpscOutput` (`is_critical`, `is_on_battery`, `reports_utility_power`) have zero call sites anywhere in the repo, and their doc comments assert caller relationships that do not exist -- the live UPS safety path routes through `UpscOutput::severity()` instead.

**Before:**

~~~~
cli/src/parse/types.rs:769-791 defines three predicates whose docs name callers:
```
/// True when the UPS is reporting any critical state. See
/// `UpsStatusFlag::is_critical` for the token list.
pub fn is_critical(&self) -> bool { ... }

/// ... Used
/// by preflight to refuse mutations that would start during an
/// outage, narrowing the recovery surface.
pub fn is_on_battery(&self) -> bool { ... }

/// ... See `preflight::check_ups_not_on_battery`.
pub fn reports_utility_power(&self) -> bool { ... }
```
But `preflight::check_ups_not_on_battery` (cli/src/preflight.rs:634) does `match parsed.severity() { UpsSeverity::Online => Ok(()), ... UpsSeverity::Indeterminate => refuse("UPS does not report utility power (OL missing)") }` -- it never calls any of the three.
~~~~

**After:**

~~~~
Delete all three methods from `impl UpscOutput`. `severity()` -> `UpsSeverity` is the single live classifier (used by `preflight::check_ups_not_on_battery` and `ups::cmd_ups_status`) and already encodes both the critical and the affirmative-OL rules. If any is kept for a planned caller, its `///` must stop naming a caller that does not exist (doc-comments.md bans "fabricated invariant nothing enforces").
~~~~

**Finder's evidence:** `rg -n "\.is_on_battery\(\)|\.reports_utility_power\(\)" cli/ tests/` returns nothing; `rg -n "\.is_critical\(\)" cli/` returns only cli/src/parse/types.rs:970,987, which call `UpsStatusFlag::is_critical` (line 729), not the `UpscOutput` method (line 771). A whole-repo scan of `pub` items with total identifier count <= 1 flagged `reports_utility_power` as the only truly zero-reference `pub` item in cli/src + cli/tests. Live `UpscOutput::severity()` callers: cli/src/ups.rs:263, cli/src/preflight.rs:634.

**Verifier's correction:** Core claim holds, with two refinements. (1) The stale-caller-doc charge applies to two of the three, not all three: `is_on_battery` ("Used by preflight to refuse mutations...") and `reports_utility_power` ("See `preflight::check_ups_not_on_battery`") name a relationship that commit 11ad684d dissolved; `UpscOutput::is_critical`'s doc only cross-references `UpsStatusFlag::is_critical` and is not itself fabricated -- it is merely dead. (2) The deletion must be scoped to the `impl UpscOutput` block: `UpsStatusFlag::is_critical` (types.rs:729) stays, since it is the real primitive behind `UpsSeverity::classify` and is pinned by the `ups_status_flag_critical_set` test.

**Verifier's reasoning:** Read cli/src/parse/types.rs:763-792 -- the "before" block is verbatim accurate (severity() at 765, is_critical at 771, is_on_battery at 779, reports_utility_power at 789). A whole-repo rg (including cli/tests, tests/, docs/, modules/) finds zero call sites for all three: the only non-definition `is_critical` hits are `UpsStatusFlag::is_critical` (types.rs:750 fn-ref, 970/987 on `flag`) and an unrelated test name in alert.rs:2870; `is_on_battery`/`reports_utility_power` appear only inside types.rs itself and in historical plans/impl/ files. I confirmed the live path: preflight.rs#check_ups_not_on_battery matches on `parsed.severity()` and ups.rs:263 does the same, and `git log -S` shows commit 11ad684d ("feat(cli): add shared ups severity tags") is what stranded them. Because braid-cli has a lib.rs, `pub` items get no dead_code lint, so these hide silently; nothing in docs/design/ or ADR 020 names them, and no test would break. The one "keep" decision (plans/impl/2026-06-17:51, "Keep is_critical / is_on_battery / reports_utility_power (still used / referenced) ... they remain the primitives") is self-refuting: `UpsSeverity::classify` is built on `UpsStatusFlag::is_critical` and raw `.contains()`, never on these `UpscOutput` methods -- and plans/impl is a historical record, not the authority (principles.md + decisions/) AGENTS.md points at.

#### TASK-5: `cli/src/online_state.rs:298` [medium/trivial]

*Lens: error-handling | category: swallowed-error*

**Claim:** `mark_online` discards the `OnlineError` from `systemctl_start` via `.is_err()`, so the warning omits the exit code and stderr that the error variant already carries -- the only `.is_err()`-style error swallow left in the CLI.

**Before:**

~~~~
```rust
UnitActiveState::Inactive | UnitActiveState::Failed => {
    if ops.systemctl_start(BRAID_ONLINE_UNIT).is_err() {
        eprintln!(
            "braid: WARNING: failed to activate braid-online.service -- pool is mounted but shutdown may not lock automatically"
        );
    }
}
```
~~~~

**After:**

~~~~
Bind and interpolate the error like every sibling warning in the same function:
```rust
if let Err(e) = ops.systemctl_start(BRAID_ONLINE_UNIT) {
    eprintln!(
        "braid: WARNING: failed to activate braid-online.service ({e}) -- pool is mounted but shutdown may not lock automatically"
    );
}
```
~~~~

**Finder's evidence:** `OnlineError::SystemctlStart { unit, exit_code, stderr }` (cli/src/online_state.rs:96-100) renders `systemctl start {unit} failed (exit {exit_code}): {stderr}` -- exactly the diagnostic dropped here. The three neighbouring warnings in the same function all keep their detail: line 270 `failed to check mountpoint {mount_point:?}: {e}`, line 281 `failed to set ownership on {}: {e}`, line 287 `failed to set permissions on {}: {e}`, and the adjacent `UnitActiveState::Unknown` arm (line 304) interpolates `({reason})`. `rg -n '\.is_err\(\)|\.is_ok\(\)' cli/src` shows this is the sole non-test occurrence used as a discard.

**Verifier's correction:** Minor scope note: no unit test exercises this branch today (`RecordingOnlineStateOps::systemctl_start` always returns `Ok`, and `StagedOnlineFailure` has no `SystemctlStart` variant), so the fix is message-only and no existing test needs updating.

**Verifier's reasoning:** The "before" block matches `cli/src/online_state.rs` exactly at lines 297-303, and `OnlineError::SystemctlStart` (line 97) does carry `unit`/`exit_code`/`stderr`, all captured by the real runner (`CmdRequest::SystemctlStart` goes through the generic capturing `run`), so real diagnostic content is discarded. This is not a deliberate house style: the mirror function `mark_offline` (line 358) prints `failed to deactivate braid-online.service: {e}`, the adjacent `Unknown` arm interpolates `({reason})`, and the other three warnings in `mark_online` all interpolate `{e}`; `git log -L` shows the `.is_err()` shape is a literal port of the old shell wrapper's `if ! systemctl start ... 2>/dev/null` in `modules/braid/braid-wrapper.sh`, i.e. inherited shell limitation, not design. `rg` over the whole repo shows the message text is pinned by no test, doc, or module file (only `cli/src/online_state.rs` and archived plan files), and `rg '\.is_err\(\)'` over `cli/src` confirms every other occurrence is inside `#[cfg(test)]` asserts, so this is the sole non-test discard; ADR 026 and 018 say only that unknown/failed states "warn", imposing no wording constraint, and the added `({e})` is ASCII so `check-output-ascii.py` is unaffected.

#### TASK-6: `justfile:121` [medium/trivial]

*Lens: test-gaps | category: dead-test-target*

**Claim:** `just test-rust` selects integration-test targets explicitly and omits `root_check` and `tty_passphrase`, so 10 real integration tests have never run in the Rust lane or in CI.

**Before:**

~~~~
test-rust:
    cargo test --lib --bin braid --test golden_nixos_26_05 --test tty_guard --test confirm_yes

Because explicit `--test <name>` selection suppresses all other integration targets, `cli/tests/root_check.rs` (8 tests: `non_root_exits_with_error`, `non_root_doctor_exits_with_error`, `help_works_without_root`, `help_subcommand_works_without_root`, `version_works_without_root`, `add_dry_run_flag_accepted`, `add_requires_at_least_one_disk`, `add_progress_values_accepted`) and `cli/tests/tty_passphrase.rs` (2 tests: `pty_integration`, `deadlock_immunity`) never execute.
~~~~

**After:**

~~~~
    cargo test --lib --bin braid --test golden_nixos_26_05 --test tty_guard --test confirm_yes --test root_check --test tty_passphrase

Both files still compile against current APIs (`braid_cli::luks::read_tty_from_file` at cli/src/luks.rs:198, `Secret::expose_secret` at cli/src/secret.rs:19) and against the current clap surface (`AddArgs.disks` is `#[arg(required = true, num_args(1..))]` at cli/src/main.rs:322, matching `add_requires_at_least_one_disk`).
~~~~

**Finder's evidence:** `git log -S '--test tty_guard' -- justfile` shows commit 931897a3 established the pattern of appending a `--test <name>` when a new integration test lands; root_check.rs and tty_passphrase.rs predate it and were never added. `rg -n 'root_check|tty_passphrase'` across the repo finds zero references outside the files themselves -- no justfile recipe, no flake.nix check, no .github workflow. The only CI that runs Rust tests is .github/workflows/release.yml:67 (`nix develop --command just test-rust`, tag-triggered), which inherits the same omission; .github/workflows/checks.yml:89 runs a single named `cargo test --lib` case. These tests guard live contracts: the `(s)` literal-plural-marker rule from commit ff8235a8, the root gate whose defense-in-depth arm was deliberately deleted from check_beep_path, and the rpassword deadlock/termios-restore regressions from commits 5f8aee6d / cc95789d.

**Verifier's correction:** `just test-rust` (justfile:121) does omit `root_check` and `tty_passphrase`, so those 10 passing integration tests never run in the Rust lane -- including release.yml's `nix develop --command just test-rust` gate. They are not, however, dead in CI overall: crane's `buildPackage` runs with `doCheck=1` and `checkPhase = cargoWithProfile test --locked`, so `nix build .#packages.x86_64-linux.braid-cli-unwrapped` (release build step, and any VM check that builds the package) executes every integration target. Also, only `root_check.rs` (2026-02-24) predates the allowlist commit 26e02547 (2026-04-03); `tty_passphrase.rs` was added later (5f8aee6d, 2026-04-24) and was simply never registered.

**Verifier's reasoning:** The "before" is exact: justfile:121 is `cargo test --lib --bin braid --test golden_nixos_26_05 --test tty_guard --test confirm_yes`, and I confirmed empirically that this explicit target allowlist runs only main.rs/confirm_yes/tty_guard while the "after" form additionally runs `tests/root_check.rs` (8 tests) and `tests/tty_passphrase.rs` (2 tests) -- all 10 pass locally in ~1.1s, so the fix is behavior-preserving and adds real coverage of the root gate, the `(s)` plural-marker rule, clap's `add` required-args refusal, and the termios/deadlock passphrase reader. The omission is oversight, not policy: the recipe comment justifies only "excludes unstable golden tests" (golden_nixos_unstable, which the after correctly still omits), later commits kept appending `--test tty_guard`/`--test confirm_yes` as new targets landed, and two plan docs (`plans/impl/2026-05-14-cli-drop-literal-s-pluralization.md`, `plans/impl/2026-05-26-discover-expect-count-requires-write.md`) explicitly treat root_check.rs assertions as gated by `just test-rust`, which is false today. Nothing in AGENTS.md, principles.md, or the ADRs sanctions the exclusion. The one overstatement: these tests are not dead in CI -- `nix derivation show .#packages...braid-cli-unwrapped` has `doCheck=1` with `checkPhase = cargoWithProfile test --locked`, so the release workflow's `nix build .#packages.x86_64-linux.braid-cli-unwrapped` (and every VM check that builds the crane package) does execute them; the gap is the fast Rust lane and the pre-build `just test-rust` release gate.

### Medium value, small effort

#### TASK-7: `cli/src/lib.rs:66` [medium/small]

*Lens: dead-dup+doc-drift | category: stale-comment-dead-code-suppression*

**Claim:** The blanket `#[allow(dead_code)]` over the whole `tui` module is justified by a comment claiming the TUI is "stubbed out", which has been false for months -- it now suppresses dead-code detection across the crate's largest subtree.

**Before:**

~~~~
// TUI is stubbed out — suppress unused-code warnings for now.
// TODO: remove #[allow(dead_code)] once the TUI is more developed.
#[allow(dead_code)]
pub mod tui;
~~~~

**After:**

~~~~
Delete the two comment lines and the `#[allow(dead_code)]`, leaving `pub mod tui;`. Run `cargo clippy --manifest-path cli/Cargo.toml --tests` and delete (not re-allow) whatever the compiler then reports. If a small number of items must stay, move the allow to those items with a real reason, not to the module.
~~~~

**Finder's evidence:** `git log -S"TUI is stubbed out" -- cli/src/lib.rs` -> single commit `ac46c26a` (2026-02-25, "satisfy clippy"); `git log --oneline --since=2026-02-25 -- cli/src/tui | wc -l` -> 156 commits since. The TUI is a shipped, documented feature: README.md lists "**TUI dashboard** -- `braid tui` ..." under Features and the command table (`docs/commands/tui.md`), `cli/src/tui/` is ~15k lines across app.rs/view/browse/probe, and flake.nix registers `braid-tui-browse` and `braid-tui-browse-ups-discovery` VM checks. I scanned every `fn` and struct field under `cli/src/tui/` for zero references and found none, so the allow is currently masking nothing -- which is exactly why it should come off before it starts to.

**Verifier's correction:** The finding's core survives, but its evidence is wrong on one point in the direction that strengthens it: the allow is NOT currently masking nothing. Force-warning `dead_code` reveals four suppressed items — `BrowseState::is_subvolume_detail` in `cli/src/tui/browse/state.rs` plus the `probed_at` fields on `FanSnapshot`, `UpsSnapshot`, and `PoolState` in `cli/src/tui/model.rs`. So the "after" should expect real cleanup, not a no-op: the three `probed_at` fields are written at ~15 construction sites (app.rs, probe.rs, view/mod.rs, browse/state.rs, browse/view.rs, demo.rs) and never read, and `is_subvolume_detail` is referenced only from three `#[cfg(test)]` assertions in its own file, so it needs either deletion together with those assertions or a `#[cfg(test)]`/reasoned per-item allow rather than a straight delete.

**Verifier's reasoning:** The "before" is verbatim accurate at /Users/dan/Code/braid/cli/src/lib.rs lines 66-68 (comment em-dash and all; comments are exempt from the ASCII rule, so no convention conflict). The staleness is real, not reworded: `git log -S"TUI is stubbed out" -- cli/src/lib.rs` shows a single commit ac46c26a ("satisfy clippy", 2026-02-25) with 156 commits to cli/src/tui since, ~14k lines across 15 files, `braid tui` documented in README/docs/commands/tui.md, and flake.nix VM checks — the TUI is not stubbed out. Decisively, I compiled with `CARGO_TARGET_DIR=<scratch> RUSTFLAGS="--force-warn dead_code" cargo check --tests --offline` (force-warn overrides `allow` without editing anything): the module allow is masking four real items — `cli/src/tui/browse/state.rs#BrowseState::is_subvolume_detail` (never used outside `#[cfg(test)]` assertions) and the never-read `probed_at` fields on `FanSnapshot`, `UpsSnapshot`, and `PoolState` in `cli/src/tui/model.rs`. Baseline `cargo clippy --manifest-path cli/Cargo.toml --tests` is currently zero-warning, no ADR or doc sanctions the blanket allow, and the in-tree TODO asks for exactly this removal, so nothing deliberate is being fought.

#### TASK-8: `cli/src/mount.rs:703` [medium/small]

*Lens: dead-dup | category: duplicated-logic*

**Claim:** The "scope `btrfs device scan --forget` to existing close-set paths, warn and continue on failure" block is implemented twice, byte-for-byte identical including both operator-facing warn strings.

**Before:**

~~~~
cli/src/mount.rs (close_opened_mappers):
    if !forget_devs.is_empty() {
        let forget_result = runner.run(&CmdRequest::BtrfsDeviceScanForget { devices: forget_devs });
        match forget_result {
            Ok(r) if r.exit_status == 0 => {}
            Ok(r) => { ... "btrfs device scan --forget failed (exit {}): {} (continuing)" ... }
            Err(e) => { ... "btrfs device scan --forget failed: {e} (continuing)" ... }
        }
    }
cli/src/lock.rs:722-745 repeats the same match arms and the same two format strings verbatim.
~~~~

**After:**

~~~~
Hoist a shared helper (e.g. `mapper_close::forget_scanned_devices<R, F>(runner, fs, devices, color_enabled)`) that filters by `fs.exists`, issues `CmdRequest::BtrfsDeviceScanForget`, and owns both `[warn]` rows; call it from `mount::close_opened_mappers` and from `lock.rs`'s post-umount step. Only the device-source expression (`opened.iter().map(|t| t.mapper.dev_path())` vs `self.close_set.forget_paths()`) stays at the call site.
~~~~

**Finder's evidence:** `grep -n "btrfs device scan --forget failed" cli/src/*.rs` -> `lock.rs:732`, `lock.rs:741`, `mount.rs:719`, `mount.rs:729` carry the identical `(continuing)` wording; `pool.rs:218` and `recover.rs:3482` are deliberately different (they hard-fail rather than warn), so only these two are duplicates. This matches the module's existing pattern of single-sourcing shared row wording -- `cli/src/mapper_close.rs#CloseContext::row_suffix` and `cli/src/luks.rs#mapper_conflict_found_display` both exist for exactly this reason.

**Verifier's correction:** The two blocks are semantically identical and share both operator-facing format strings verbatim, but they are not literally byte-for-byte: `mount.rs` calls `status_line(StatusTag::Warn, color_enabled, ...)` directly while `lock.rs` goes through its local `line` closure, and the existence filter is written as `.filter(|path| fs.exists(path))` inside the collect in mount.rs versus `forget_devs.retain(|p| fs.exists(p))` in lock.rs. Also worth noting: only lock.rs (and recover.rs) carries the comment explaining why the forget is scoped to the close set rather than the kernel-global no-arg form; mount.rs has no such comment, so the hoist would additionally single-source that rationale.

**Verifier's reasoning:** I read `cli/src/mount.rs#close_opened_mappers` (block starts exactly at line 703) and `cli/src/lock.rs` LockPlan::execute (lines 720-745): both filter close-set paths by `fs.exists`, issue `CmdRequest::BtrfsDeviceScanForget`, and emit the same two `[warn]` rows with character-identical format strings ("btrfs device scan --forget failed (exit {}): {} (continuing)" and "btrfs device scan --forget failed: {e} (continuing)"), then continue. `rg` confirms the only other sites (`pool.rs:218`, `recover.rs:3482`) hard-fail with different wording, so they are genuinely not part of the dup, and `rg "fn .*forget"` shows no existing shared helper. Nothing marks the split as deliberate: no ADR or comment distinguishes them (ADR 022/024 and docs/commands/lock.md + docs/internals/luks-unlock.md describe the same warn-and-continue contract for both), and `cli/src/mapper_close.rs#CloseContext` carries a doc comment stating the repo hoists shared close-row wording precisely to stop it drifting -- direct precedent for the proposed helper. Both pinning tests (`lock_forget_failure_is_nonfatal`, `cleanup_forget_failure_warns_and_still_closes_all_mappers`) assert on mock requests and captured output, so a behavior-preserving hoist keeps them green.

#### TASK-9: `cli/src/luks.rs:116` [medium/small]

*Lens: dead-dup | category: duplicated-logic*

**Claim:** Three user-facing mapper-ownership error messages are duplicated verbatim between `LuksError` and `ProbeError`, despite the codebase already centralizing one sub-fragment of them precisely to stop this pair from drifting.

**Before:**

~~~~
cli/src/luks.rs:116-147 (`LuksError::MapperConflict` / `MapperBackingMismatch` / `MapperBackingResolveError`) and cli/src/probe.rs:90-121 (`ProbeError::MapperConflict` / `MapperBackingMismatch` / `MapperBackingResolveError`) carry byte-identical `#[error(...)]` bodies, e.g. both:
    #[error(
        "disk '{name}' mapper '/dev/mapper/braid-{name}' is open but not \
         backed by the configured disk. Expected LUKS UUID {expected}, \
         found {}. Close the conflicting mapper with \
         'sudo cryptsetup close braid-{name}' and re-run.",
        mapper_conflict_found_display(found)
    )]
~~~~

**After:**

~~~~
Keep both enums' variants (the exhaustive `ProbeError` matches in `lock.rs`, `monitor.rs`, `status.rs`, and `tui/probe.rs` are a deliberate compile-time gate), but move the three message bodies next to the existing `mapper_conflict_found_display` in `luks.rs` as `pub(crate) fn mapper_conflict_message(name, expected, found) -> String`, `mapper_backing_mismatch_message(...)`, `mapper_backing_resolve_message(...)`, and have both enums render via `#[error("{}", mapper_conflict_message(name, expected, found))]`. Behavior-preserving; the format-expression form is already used by these variants.
~~~~

**Finder's evidence:** `cli/src/luks.rs#mapper_conflict_found_display` is documented as "Centralized mapper-conflict backing rendering so probe and LUKS errors preserve the same LUKS UUID wording" -- the intent to single-source already exists, it just stops at one sub-fragment while the surrounding 4-line sentences are copied. Both variant sets are produced from the same `OwnershipError` via two near-identical `From` impls (`cli/src/luks.rs:870-903` and `cli/src/probe.rs:128-161`), confirming one behavior with two renderings.

**Verifier's correction:** Line numbers drifted by one: the duplicated blocks are cli/src/luks.rs:117-149 and cli/src/probe.rs:91-123 (the `From<OwnershipError>` impls are luks.rs:870-904 and probe.rs:128-162). Also, drift is only partially guarded today: exact-string Display tests exist for `MapperConflict` on both sides (luks.rs:1255-1287, probe.rs:680-720), but nothing asserts the two renderings are equal to each other, and `MapperBackingMismatch`/`MapperBackingResolveError` have no exact-string Display test at all -- those two are the genuinely unguarded copies.

**Verifier's reasoning:** I diffed the two regions directly: `sed -n '117,149p' cli/src/luks.rs` vs `sed -n '91,123p' cli/src/probe.rs` is byte-identical, so all three `#[error(...)]` bodies plus field declarations are verbatim copies, and both are produced from the same `OwnershipError` by two mirror-image `From` impls (`cli/src/luks.rs:870-904`, `cli/src/probe.rs:128-162`). The repo's own intent points at deduplication rather than away from it: `cli/src/luks.rs#mapper_conflict_found_display` is documented as centralizing this exact pair, `plans/impl/2026-05-26-unify-mapper-conflict-display.md` did that refactor while forbidding wording changes (which the proposed fix honors), and the paired tests say "probe and LUKS mapper-conflict errors share wording". I found no ADR, principle, or code comment sanctioning the duplication; `docs/design/decisions/024-luks-uuid-identity.md` and the AGENTS.md error-prefix convention are untouched by the change (none of these six variants carries a subsystem tag, and that stays true). The exhaustive `ProbeError::Mapper*` matches in `lock.rs:868`, `monitor.rs:90`, `status.rs:1079`, and `tui/probe.rs:408-435` are real behavioral gates, and the proposal correctly keeps both variant sets intact; the `#[error("{}", f(field))]` form is proven to compile here since luks.rs:122 already uses a bare field binding in a trailing format expression. Note `cli/src/replace.rs:95-115` maps the same `OwnershipError` to deliberately re-worded variants, which is a legitimate per-context difference and is not swept up by this fix.

#### TASK-10: `cli/src/parse/btrfs_device_usage.rs:162` [medium/small]

*Lens: dead-dup | category: duplicated-logic*

**Claim:** The same six-line `fn fixture(name: &str) -> String` test helper is copy-pasted byte-for-byte into 13 parser modules, all resolving the identical `tests/fixtures/nixos-26.05/` path.

**Before:**

~~~~
    fn fixture(name: &str) -> String {
        let path = format!(
            "{}/tests/fixtures/nixos-26.05/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("fixture {name}: {e}"))
    }
~~~~

**After:**

~~~~
Define it once under `#[cfg(test)]` in `cli/src/parse/helpers.rs` (which already exists for shared parser helpers) as `pub(super) fn fixture(name: &str) -> String`, and replace the 13 local copies with `use super::super::helpers::fixture;` (or a `use crate::parse::helpers::fixture;`). Leave `smartctl.rs` (uses a `FIXTURE_DIR` const for the same dir -- fold it in too) and `systemctl_list_units.rs` (deliberately reads the un-versioned `tests/fixtures/` root for a hand-authored fixture) alone or migrate them explicitly.
~~~~

**Finder's evidence:** `rg -n "fn fixture\(" cli/src/parse` lists 15 definitions; diffing them shows 13 identical (`btrfs_balance_status`, `btrfs_device_stats`, `btrfs_device_usage`, `btrfs_filesystem_df`, `btrfs_filesystem_show`, `btrfs_filesystem_usage`, `btrfs_scrub_status`, `btrfs_scrub_status_per_device`, `btrfs_subvolume_list`, `cryptsetup_luks_dump`, `cryptsetup_luks_uuid`, `cryptsetup_status`, `lsblk`). This matters under AGENTS.md's parser-compatibility rule: a fixture-dir move on a nixpkgs bump is currently a 13-site edit. `cli/src/parse/helpers.rs` is already the established home for cross-parser helpers (`parse_ctime`, `parse_duration_hms`).

**Verifier's correction:** Accurate as stated, with two small additions: the fixture-dir literal is also duplicated a 14th time outside `parse/` in `cli/src/test_fixtures/doctor.rs` (`let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/nixos-26.05")`), and the concrete precedent for the "13-site edit" claim is commit `9d237f7b`, which renamed the lane from `nixos-25.11` and touched every parser module's path literal.

**Verifier's reasoning:** Read `cli/src/parse/btrfs_device_usage.rs` at line 162 -- the "before" snippet is byte-accurate. Hashing the extracted helper from all 13 named modules yields the identical md5 (059bd354...), so the copies are truly byte-for-byte, and `rg` confirms only `smartctl.rs` (FIXTURE_DIR const, same dir) and `systemctl_list_units.rs` (un-versioned `tests/fixtures/` root) differ, exactly as the candidate says. `cli/src/parse/mod.rs` already declares `mod helpers;` holding `pub(super)` cross-parser helpers (`parse_ctime`, `parse_duration_hms`), and `pub(super)` there is `pub(in crate::parse)`, which sibling modules' `mod tests` can import -- so the proposed home compiles and breaks nothing; `docs/dev/doc-comments.md` explicitly exempts `#[cfg(test)]` items, so no doc-comment burden. The maintenance cost is not hypothetical: commit `9d237f7b` ("bump nixpkgs pin to nixos-26.05") renamed `nixos-25.11` -> `nixos-26.05` and had to edit the path literal in every one of these parser modules, and the repo already shows the opposite-of-deliberate stance by centralizing test helpers elsewhere (`cli/src/test_fixtures/shared.rs`). No principle or Active ADR touches this; it is test-only code with zero behavior surface.

#### TASK-11: `cli/src/test_fixtures.rs:165` [medium/small]

*Lens: dead-dup | category: dead-code*

**Claim:** Six fixture re-exports in `test_fixtures.rs` are used by no test module -- they are consumed only inside their own defining fixture file -- and the `#[allow(unused_imports)]` on those blocks is what keeps the compiler from saying so.

**Before:**

~~~~
#[allow(unused_imports)]
pub(crate) use idle::{ ... idle_runner_with_scrub_finished, idle_scrub_finished, ... };
...
#[allow(unused_imports)]
pub(crate) use monitor::{ ... usage_2disk, usage_2disk_healthy, usage_2disk_one_missing, ... };
...
#[allow(unused_imports)]
pub(crate) use status::{ status_btrfs_device_stats_3disk, ... status_btrfs_device_usage_raw_3disk, status_btrfs_df_raid1, ... status_btrfs_show_3disk, ... };
~~~~

**After:**

~~~~
Drop `idle_scrub_finished`, `usage_2disk_healthy`, `status_btrfs_device_stats_3disk`, `status_btrfs_device_usage_raw_3disk`, `status_btrfs_df_raid1`, and `status_btrfs_show_3disk` from the re-export lists (the functions themselves stay -- they are used within `idle.rs`, `monitor.rs`, and `status.rs`), then remove `#[allow(unused_imports)]` from every block that no longer needs it and let the compiler police the lists from here on.
~~~~

**Finder's evidence:** For each of the six, `rg -n -w <name> cli/src cli/tests tests --glob '!cli/src/test_fixtures.rs' --glob '!cli/src/test_fixtures/**'` returns zero hits; their only uses are internal (e.g. `idle_scrub_finished` at `cli/src/test_fixtures/idle.rs:238`, `usage_2disk_healthy` at `monitor.rs:238/247/256/267/307`, `status_btrfs_show_3disk` at `status.rs:492`). Four of the seventeen re-export blocks (`enroll_key_file`, `lock`, `mount`, `replace`) carry no allow and have no dead names, showing the un-suppressed form is the intended state.

**Verifier's correction:** The dead facade re-exports are nine, not six: add `RecoverParamsBuilder`, `RemoveParamsBuilder`, and `RemoveMissingParamsBuilder` (the builder types stay -- tests reach them by method chaining off `PoolFixture`, never by name). With all nine names dropped, all 13 `#[allow(unused_imports)]` attributes can go and `cargo check --tests` is warning-free; dropping only the candidate's six would leave the recover/remove/remove_missing blocks still needing their allow.

**Verifier's reasoning:** The "before" matches current source: `/Users/dan/Code/braid/cli/src/test_fixtures.rs` line 165 is the `#[allow(unused_imports)]` on the `idle` re-export block, and 13 of the 17 facade blocks carry that attribute. Rather than trust grep, I copied the workspace to scratch, stripped every `#[allow(unused_imports)]`, and ran `cargo check --tests`: it compiled clean except for unused-import warnings naming exactly the candidate's six (`idle_scrub_finished`, `usage_2disk_healthy`, `status_btrfs_device_stats_3disk`, `status_btrfs_device_usage_raw_3disk`, `status_btrfs_df_raid1`, `status_btrfs_show_3disk`) plus three the candidate missed (`RecoverParamsBuilder`, `RemoveParamsBuilder`, `RemoveMissingParamsBuilder`) -- so the six are genuinely facade-dead, still live inside their own fixture modules, and no dead_code fallout appears. This is not deliberate: `plans/impl/2026-05-26-test-fixtures-dead-code.md` names these same "~13 `#[allow(unused_imports)]` blocks on the facade re-exports" as the same staged-migration leftover on a different lint and explicitly defers them to "a separate pass"; nothing in AGENTS.md, principles.md, or the ADRs sanctions keeping unused re-exports (the facade module doc only justifies flat/prefixed naming, not stale exports). Test-only change, zero behavior impact.

#### TASK-12: `cli/src/replace.rs:312` [medium/small]

*Lens: control-flow | category: duplication*

**Claim:** `ReplaceWorkPlan::render_steps`'s `ExistingLuks` arm re-implements, byte-for-byte, the two preview steps that `add.rs` already factored into the `push_returned_disk_enrollment_steps` helper, so the two commands' dry-run wording can silently drift.

**Before:**

~~~~
replace.rs (ExistingLuks arm):
```rust
                if let Some(kf) = enroll_key_file {
                    let header_backup_path =
                        luks_header_backup_path(&self.luks_headers_dir, &self.new_mapper);
                    steps.push(Step {
                        risk: "safe",
                        description: format!("enroll keyfile -> LUKS slot 1 on {}", self.new_by_id),
                        commands: vec![CmdRequest::CryptsetupLuksAddKeyFile {
                            device: self.new_by_id.as_str().to_owned(),
                            key_file_path: kf.as_path().display().to_string(),
                        }],
                    });
                    steps.push(Step {
                        risk: "safe",
                        description: format!(
                            "LUKS header backup -> {}",
                            header_backup_path.as_path().display()
                        ),
                        commands: vec![CmdRequest::CryptsetupLuksHeaderBackup {
                            device: self.new_by_id.as_str().to_owned(),
                            backup_path: header_backup_path.as_path().display().to_string(),
                        }],
                    });
                }
```
~~~~

**After:**

~~~~
Promote `add.rs#push_returned_disk_enrollment_steps` to a shared `pub(crate)` location (it already takes exactly `(&mut Vec<Step>, &ByIdPath, &KeyFilePath, &HeaderBackupPath)`) and call it here:
```rust
                if let Some(kf) = enroll_key_file {
                    let header_backup_path =
                        luks_header_backup_path(&self.luks_headers_dir, &self.new_mapper);
                    push_returned_disk_enrollment_steps(
                        &mut steps,
                        &self.new_by_id,
                        kf,
                        &header_backup_path,
                    );
                }
```
~~~~

**Finder's evidence:** `cli/src/add.rs#push_returned_disk_enrollment_steps` (add.rs:873-898) builds the identical pair: same `risk: "safe"`, same `format!("enroll keyfile -> LUKS slot 1 on {}", by_id)` / `format!("LUKS header backup -> {}", header_backup_path.as_path().display())` descriptions, same `CryptsetupLuksAddKeyFile` / `CryptsetupLuksHeaderBackup` payloads, same order. Types line up: `luks.rs#luks_header_backup_path` returns `HeaderBackupPath`, and replace's `enroll_key_file` is `Option<KeyFilePath>`. ADR 022 (dry-run-preview-model) makes previews the shared source of truth for execution, and AGENTS.md's "reach for the ideal, robust, simple" plus the repo's recurring "single source so X and Y cannot drift" comments make one helper the house shape. Note the `FreshLuks` arm is NOT a candidate: there the header backup is unconditional while enrollment is conditional.

**Verifier's correction:** Core claim holds, with two scope caveats: (1) the helper must be promoted to a neutral home (`cli/src/luks.rs` or `cli/src/cmd.rs`) rather than imported from `add.rs` -- no command module currently imports from another command module -- and its doc comment ("for a returned-disk add target") needs rewording; (2) fixing only replace.rs does not achieve a single source of truth: `cli/src/enroll_key_file.rs#compile_enroll_steps` (lines 466-485) contains a third byte-identical copy of the same pair and should be routed through the same helper in the same change. The two Fresh arms (`add.rs` Fresh, `replace.rs` FreshLuks) correctly stay inline since their header backup is unconditional.

**Verifier's reasoning:** I read `cli/src/replace.rs` lines 308-345 and confirmed the "before" is verbatim current source, and `cli/src/add.rs#push_returned_disk_enrollment_steps` (lines 873-898) builds the identical pair field-for-field (same `risk: "safe"`, same two `format!` strings, same `CryptsetupLuksAddKeyFile`/`CryptsetupLuksHeaderBackup` payloads, same addKey-before-backup order); types line up (`ReplaceTargetPrep::ExistingLuks.enroll_key_file: Option<KeyFilePath>`, `new_by_id: ByIdPath`, `luks_header_backup_path -> HeaderBackupPath`), so the swap is behavior-preserving and is guarded by the existing ExistingLuks render test in replace.rs (~line 4861, which pins addKey/headerBackup ordering and both stringly fields). Nothing in AGENTS.md, `docs/design/principles.md`, or ADR 022 opposes it -- ADR 022 explicitly praises "one shared `cli/src/cmd.rs#luks_format_argv`", and `cli/src/luks.rs` carries a test comment that "the recovery hint is shared across enroll, add, and replace, so wording drift must fail at the helper boundary", making a shared helper the house shape; `plans/impl/2026-06-10-keyfile-headerbackup-path-newtypes.md` itself calls this exact dedup "worthwhile but orthogonal", i.e. deferred, not deliberate. `luks.rs` already imports `CmdRequest`, so promoting the helper there (or to `cmd.rs`) as `pub(crate)` with a reworded `///` costs little; note that a straight `use crate::add::...` would be the only cross-command-module import in `cli/src`, so the promotion (not a direct import) is the right form.

#### TASK-13: `cli/src/preview.rs:9` [medium/small]

*Lens: api-naming | category: dead-code*

**Claim:** The `PreviewCompleteness`/`PreviewGap` scaffolding never materialized: `Partial` is constructed only in one test, `PreviewGap` is still uninhabited, the render branch is an empty loop with commented-out code, and the three `PR 0` doc claims are now false since every mutating command has migrated to `Preview`.

**Before:**

~~~~
Module doc (lines 9-10): `//! PR 0 lands the types and rendering primitives only -- no command\n//! migrations.`
Line 43: `pub enum PreviewGap {}`
Lines 292-301:
```
if let PreviewCompleteness::Partial { reasons } = &self.completeness {
    for _reason in reasons {
        // PreviewGap is uninhabited in PR 0; this body is
        // unreachable today. The first variant adds:
        //   out.push_str(&format!(
        //       "note: preview incomplete -- {}\n",
        //       reason.label(),
        //   ));
    }
}
```
All eight production `Preview { ... }` literals write `completeness: PreviewCompleteness::Complete`.
~~~~

**After:**

~~~~
Drop the `completeness` field, `PreviewCompleteness`, and `PreviewGap`, plus the unreachable render branch and the `Partial`-with-zero-reasons test; the eight plan `preview()` bodies lose a field that can only hold one value. Rewrite the module doc to describe the shipped state (all mutating commands render through `Preview`). If the placeholder is deliberately retained, the three `PR 0` claims must at minimum be reworded -- the migrations they say have not happened have happened.
~~~~

**Finder's evidence:** `rg -n "PreviewCompleteness::Partial" cli/src` -> only preview.rs:292 (the render match) and preview.rs:503 (a test). `rg -n -A3 "^\s*Preview \{" cli/src` shows `completeness: PreviewCompleteness::Complete` at add.rs:1040, enroll_key_file.rs:526, lock.rs:644, recover.rs:1230, remove.rs:243, remove_missing.rs:142, replace.rs:412, unlock.rs:66 -- i.e. the migrations the module doc denies. docs/design/decisions/022-dry-run-preview-model.md (status: Active) never mentions `PreviewCompleteness`, `Partial`, or `PreviewGap`, so no ADR requires the placeholder.

**Verifier's correction:** The "three `PR 0` doc claims" undercounts slightly: preview.rs carries five `PR 0` references -- the module doc (line 9), the `PreviewGap` doc (line 37), the `Preview::render` doc step 3 (line 247), the inline comment inside the dead loop (line 294), and the test preamble (line 487). All five go away (or need rewording) under the proposed removal. Also worth noting for scoping: ADR 022 still describes `unlock` and `enroll` as "older dry-run seams" with respect to the *typed work-plan* model, but both nonetheless build a `Preview` via `preview()`, so the module doc's "no command migrations" claim is false regardless of how one reads that ADR sentence.

**Verifier's reasoning:** I read `/Users/dan/Code/braid/cli/src/preview.rs` in full and the "before" is byte-accurate: line 9-10 still says "PR 0 lands the types and rendering primitives only -- no command migrations", `pub enum PreviewGap {}` is uninhabited at line 43, and lines 292-301 are a `for _reason in reasons {}` loop whose entire body is commented out. `rg -n "completeness" cli/src` returns exactly the eight production literals the candidate lists (add.rs:1040, enroll_key_file.rs:526, lock.rs:644, recover.rs:1230, remove.rs:243, remove_missing.rs:142, replace.rs:412, unlock.rs:66) -- all `PreviewCompleteness::Complete` -- plus preview.rs's own tests, so `Partial` is constructible only in `render_partial_with_no_reasons_matches_complete` and the field is a one-valued field; the plan that introduced it (`plans/impl/2026-04-24-dry-run-preview-refactor.md`) called for all eight commands to migrate, and they have, so the doc's "no command migrations" is flatly false and the stated trigger for `PreviewGap`'s first variant ("lands alongside the first migration that needs to surface incompleteness") passed without firing. ADR 022 (status Active) never mentions `PreviewCompleteness`/`PreviewGap`/`Partial`, no `docs/` or `plans/wip/` file references them, and nothing in the CLI serializes `Preview` to JSON today (the `Serialize` derive is unused pending a future `--format json`), so removal breaks no documented contract, no consumer, and no user-visible byte; AGENTS.md's "reach for the ideal, robust, simple solution regardless of refactor cost" cuts toward removal rather than against it.

#### TASK-14: `cli/src/probe.rs:17` [medium/small]

*Lens: api-naming | category: module-placement*

**Claim:** The crate-wide `Filesystem` seam lives in `probe.rs` under a banner that describes only `Path::exists()`, so unrelated modules must import `crate::probe::Filesystem` to do filesystem work that has nothing to do with probing.

**Before:**

~~~~
```
// ---------------------------------------------------------------------------
// Filesystem trait — abstracts Path::exists() for testability
// ---------------------------------------------------------------------------

pub trait Filesystem {
    fn exists(&self, path: &str) -> bool;
    fn is_block_device(&self, path: &str) -> bool;
    fn list_dir(&self, path: &str) -> Result<Vec<String>, std::io::Error>;
    fn read_to_string(&self, path: &str) -> Result<String, std::io::Error>;
    ...
    fn create_dir_all(&self, path: &str) -> Result<(), std::io::Error>;
}
```
~~~~

**After:**

~~~~
Move `Filesystem` + `RealFilesystem` to their own `cli/src/filesystem.rs` module with a banner that states what they actually are (the crate's single filesystem seam: four reads plus `create_dir_all`, the mount/pool layer's one mutation), re-export nothing from `probe`, and update imports. At minimum, correct the banner: the trait no longer "abstracts Path::exists()" and is no longer read-only.
~~~~

**Finder's evidence:** `rg -n "use crate::probe::[^;]*;" cli/src` shows four non-test modules importing `Filesystem` and nothing else from probe -- mount_check.rs:3, idle.rs:4, preflight.rs:18, util.rs:4 -- plus seven test_fixtures modules (shared, mount, status, recover, doctor, idle, monitor, ack). The trait's own `create_dir_all` doc (probe.rs:25-30) calls it "the mount/pool execute layer's one filesystem mutation", contradicting the read-only framing of the banner three lines above.

**Verifier's correction:** Two details are off. (1) The banner never uses "read-only" framing -- the contradiction is that it names only `Path::exists()` while the trait carries five methods including one mutation; "read-only" appears only inside the `create_dir_all` doc describing read-only test doubles. (2) The evidence says "seven test_fixtures modules" but lists and `rg` finds eight (shared, mount, status, recover, doctor, idle, monitor, ack). Two facts strengthen the finding beyond what was offered: `probe.rs` and `mount_check.rs` are mutually dependent (probe imports `mount_check::MountInfoError`/`fstype_at_mount_via_fs`, mount_check imports `probe::Filesystem`), a cycle the extraction breaks; and `pub trait Filesystem` / `pub struct RealFilesystem` carry only a `//` banner, no `///`, which AGENTS.md's doc-comment convention requires for every `pub` item.

**Verifier's reasoning:** The "before" is verbatim accurate at `cli/src/probe.rs:16-32`: the banner still reads "Filesystem trait — abstracts Path::exists() for testability" while the trait now has five methods, one of which (`create_dir_all`) its own doc calls "the mount/pool execute layer's one filesystem mutation" -- so the banner is factually stale, not merely differently worded. `rg` confirms 30 files import the seam out of `probe`, four non-test modules (`mount_check.rs:3`, `idle.rs:4`, `preflight.rs:18`, `util.rs:4`) import `Filesystem` and nothing else from probe, and there is a genuine two-way module cycle the candidate missed: `probe.rs` uses `crate::mount_check::{MountInfoError, fstype_at_mount_via_fs}` while `mount_check.rs` imports `crate::probe::Filesystem`. No principle, ADR, or doc assigns the seam to `probe` (ADR 016 only references "the existing `Filesystem` abstraction"), the crate already favors tiny single-purpose modules (`by_id.rs`, `status_tag.rs`, `state_paths.rs`, `mount_check.rs`), and AGENTS.md explicitly licenses refactors regardless of scope; the move is mechanical (import-path churn only) and breaks no behavior or test.

#### TASK-15: `cli/src/remove_missing.rs:249` [medium/small]

*Lens: error-handling | category: error-taxonomy*

**Claim:** `remove-missing`'s post-commit failures (pool.json persist, journal rewrite, journal clear) all collapse into the generic `Validation` variant with no recovery remediation, unlike the structurally identical `remove`, which has dedicated variants that tell the operator to run `braid recover`.

**Before:**

~~~~
```rust
        // Membership committed by btrfs device remove. Persist before the
        // post-remove soft balance; the journal still covers maintenance,
        // so recovery can replay it if we crash before clear_journal.
        membership::save_membership(&target_membership, params.paths).map_err(|e| {
            RemoveMissingError::Validation(format!("failed to persist pool membership: {e}"))
        })?;
```
and at line 281:
```rust
        journal::clear_journal(params.paths)
            .map_err(|e| RemoveMissingError::Validation(e.to_string()))?;
```
Both are below the irreversible `pool_remove_missing_device` call (line 232), so the operator is left in recovery mode with a message that never says so.
~~~~

**After:**

~~~~
Add `MembershipPersistFailure(String)` and `JournalClearFailure(String)` to `RemoveMissingError` with the same pinned wording `RemoveError` uses, and route these two sites through classifier helpers mirroring `cli/src/remove.rs#map_membership_persist_failure` / `#map_journal_clear_failure`, e.g. `pool was modified but membership persist failed: {0}\npool.json may be stale -- run \`braid recover\` to reconcile from live state.` and `pool was modified and membership persisted, but journal clear failed: {0}\nRecovery mode remains active until pending-op.json is cleared -- run \`braid recover\`.`
~~~~

**Finder's evidence:** `cli/src/remove.rs:34-40` defines `MembershipPersistFailure` and `JournalClearFailure` with exactly that remediation text, applied at `cli/src/remove.rs:481-484` at the same lifecycle point (post `pool_remove_device`, pre-`clear_journal`). docs/dev/safety-heuristics.md requires: "Split post-commit failure variants by the operator's remediation and on-disk consequence, not by implementation layer." principles.md #3 makes the same point ("the journal triggers recovery mode"). `cli/src/replace.rs:859` and `:936` share the same gap, so the fix is worth generalizing, but `remove`/`remove-missing` are the direct A/B pair.

**Verifier's correction:** Two details are off. (a) The claim lists three collapsing sites but the proposed "after" covers only two -- the `journal::rewrite_journal` site at remove_missing.rs lines 253-263 is a third post-commit `Validation` wrap and needs either its own variant or an explicit decision to fold it into the journal bucket (its on-disk consequence differs: pool.json persisted, journal still at PoolMutation). (b) "unlike the structurally identical `remove`" overstates the norm: `remove` is the outlier that got the treatment. `replace.rs:859/936` is byte-for-byte identical to remove_missing, and `add.rs:1523/1527/1584/1623` also routes save_membership through a generic `Membership(#[from])` and clear_journal through `AddError::Validation` (though add does have post-commit-specific `PostAddProbeFailed`/`AckCleanupFailed` variants). So the fix is a generalization of remove's pattern across three commands, not a repair of a remove_missing-only regression.

**Verifier's reasoning:** Read remove_missing.rs lines 229-282 and confirmed both cited sites verbatim, both below the irreversible pool_remove_missing_device call. remove.rs does define MembershipPersistFailure/JournalClearFailure with exactly the quoted remediation text, applied via map_membership_persist_failure/map_journal_clear_failure at the identical lifecycle point (lines 483-484), each backed by a unit test that binds to the real classifier; docs/dev/safety-heuristics.md line 21 makes "split post-commit failure variants by the operator's remediation and on-disk consequence" house law, and recover.rs handles both RemoveMissing journal phases so `braid recover` is the correct advice. The clear_journal path is the worst case today: JournalError::Save renders as bare "failed to write pending-op.json at ...", never telling the operator the pool was already modified or that recovery mode is now latched. Nothing pins the current wording -- RemoveMissingError is referenced only inside remove_missing.rs (no exhaustive matches to update), and the one existing test asserts only the substrings "failed to persist pool membership" and "pool.json", both preserved by the proposed message; git log shows the remove.rs variants landed as a one-file commit (1cb4b43a) with no plan and no generalization, so this is an ungeneralized fix rather than a deliberate divergence.

#### TASK-16: `cli/src/parse/smartctl.rs:412` [medium/small]

*Lens: test-gaps | category: dead-test*

**Claim:** `nvme_fixture_healthy` reads a fixture that does not exist and can never be captured, so it silently returns without asserting anything on every run.

**Before:**

~~~~
    #[test]
    fn nvme_fixture_healthy() {
        let path = format!("{FIXTURE_DIR}/smartctl-nvme-healthy.json");
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("SKIP: fixture not captured yet");
                return;
            }
            Err(e) => panic!("reading fixture: {e}"),
        };
        assert_eq!(parse_smartctl(&raw(&content)).health, SmartHealth::Healthy);
    }
~~~~

**After:**

~~~~
Either commit a hand-authored `cli/tests/fixtures/nixos-26.05/smartctl-nvme-healthy.json` (matching how the `smartctl-selftest-*.json` fixtures are produced) and make the read unconditional (`expect`, like the sibling `fixture()` helper at line 407), or delete the test and cite `cli/src/parse/smartctl.rs#nvme_healthy_evidence_and_health`, which already pins healthy-NVMe parse with an inline JSON literal.
~~~~

**Finder's evidence:** `ls cli/tests/fixtures/nixos-26.05/ | grep smartctl` lists only `smartctl-sata-with-temperature.json` and the 11 `smartctl-selftest-*.json` files -- no `smartctl-nvme-healthy.json`; `find cli -name 'smartctl-nvme*'` returns nothing. docs/dev/parser-compatibility.md states smartctl fixtures are stable-only and that `just capture-all-fixtures` does not regenerate them, so the "not captured yet" skip can never resolve on its own. docs/dev/testing.md's "Regression test quality" says a dead test whose name points at a real contract should be replaced, not left dead. The healthy-NVMe contract is already covered unconditionally by `nvme_healthy_evidence_and_health` (cli/src/parse/smartctl.rs:1066).

**Verifier's correction:** "Can never be captured" is overstated: the fixture could be hand-authored or taken as a one-time physical-drive capture, exactly as `smartctl-selftest-*.json` and `smartctl-sata-with-temperature.json` were. The accurate claim is that no automated capture recipe produces it (`just capture-all-fixtures` explicitly skips smartctl), so the "not captured yet" skip can never resolve on its own and the test has asserted nothing since it was introduced in commit 8a6a19c4. Per docs/dev/testing.md's "replace by default" rule, the hand-authored-fixture option is preferable to deletion, since an inline JSON literal cannot detect smartmontools JSON-shape drift the way a real capture can.

**Verifier's reasoning:** The "before" matches `/Users/dan/Code/braid/cli/src/parse/smartctl.rs` lines 411-423 verbatim. `git ls-files` and `git log --all -- 'cli/tests/fixtures/nixos-26.05/smartctl-nvme-healthy.json'` show the fixture has never existed in history and `git check-ignore` shows it is not ignored, so the NotFound arm fires on every run and the test asserts nothing. docs/dev/parser-compatibility.md (lines 16-22) confirms smartctl fixtures are stable-only, that `just capture-all-fixtures` does not produce them, and that `smartctl-selftest-*.json` are hand-authored -- so no automated recipe will ever resolve the skip. This is not a house pattern: the deliberate skip idiom lives in the `golden_test!` macro in `cli/tests/support/golden_common.rs` for fixtures the VM pipeline actually captures, while the one committed smartctl golden (`cli/tests/golden_nixos_26_05.rs#golden_smartctl_sata_with_temperature`) deliberately panics on missing with a comment saying it "is part of the stable contract"; the dead test also lacks the mandatory Intent/Why/Scenario preamble its siblings carry, marking it a leftover stub. docs/dev/testing.md ("Regression test quality", lines 91-93) prescribes replacing a dead test whose name points at a real contract, and `nvme_healthy_evidence_and_health` (line 1066) already pins healthy-NVMe parse unconditionally -- both branches of the proposed "after" are conforming.

#### TASK-17: `tests/repro/remove-without-balance.py:56` [medium/small]

*Lens: test-gaps | category: missing-live-tool-lock*

**Claim:** The `stderr.contains("unable to go below")` classifier that produces braid's min-devices recovery hint has no live-tool behavior lock; the repro that reproduces the exact scenario discards btrfs's stderr.

**Before:**

~~~~
with subtest("btrfs device remove without balance fails — raid1 requires 2 devices"):
    machine.fail(
        "btrfs device remove /dev/mapper/disk2 /mnt/storage 2>&1"
    )

The test asserts only non-zero exit. `cli/src/pool.rs#device_remove_error` (line 320) branches on `stderr.contains("unable to go below")` to emit the ctx-specific recovery hint (`btrfs balance start -dconvert=raid1 ...` for Live, `braid replace` for Missing). Every test of that branch is mocked (cli/src/pool.rs:1392/1433/1470/1565/1616, cli/src/remove_missing.rs:2432, cli/src/test_fixtures/shared.rs:705) and hand-feeds the wording.
~~~~

**After:**

~~~~
Capture stderr in that subtest and assert the wording directly, e.g. `err = machine.fail("btrfs device remove /dev/mapper/disk2 /mnt/storage 2>&1")` then `assert "unable to go below" in err.lower()`. This locks the btrfs-progs wording the classifier depends on, in the same style as `tests/repro/cryptsetup-close-mounted.py`'s exit-code locks.
~~~~

**Finder's evidence:** docs/dev/testing.md#live-tool-behavior-locks: "Whenever a plan introduces a classifier of the form `exit_code == <N>` or `stderr.contains(\"<wording>\")` against an external tool, identify (or add) a live-tool repro/VM test that asserts the same code/wording directly." `rg -n 'go below' tests/` returns zero hits across all of tests/. The wording originates in vendored `reference/btrfs-progs/common/utils.h:146` (`"unable to go below two devices on raid1"`), i.e. it is upstream text braid does not control. Failure mode: a btrfs-progs rewording drops the recovery hint entirely while every mocked test still passes, leaving the operator with a bare `btrfs device remove` failure and a written pending-op journal.

**Verifier's correction:** Two details to adjust: (1) line 56 is the `with subtest(...)` header; the `machine.fail(...)` to capture is lines 57-59. (2) The lock would not be an automatic CI guard -- `flake.nix` builds `checks` as `filterAttrs (n: _: !(hasPrefix "repro-" n))`, and `.github/workflows/test.yml` enumerates `.#checks.x86_64-linux`, so `repro-remove-without-balance` runs only via `just test-repro`. That is also true of the exemplar `tests/repro/cryptsetup-close-mounted.py`, so the vehicle is still the one the doctrine prescribes, but the value is "documented, runnable lock at bump time," not "CI fails on drift." Phase 4 (`btrfs device remove missing` on the degraded raid1) discards stderr the same way and is the Missing-context analogue, though the plan notes that variant is pre-flighted out of CLI reach.

**Verifier's reasoning:** The "before" is accurate: `tests/repro/remove-without-balance.py` phase 2 (subtest header on line 56, call on 57-59) runs `machine.fail("btrfs device remove /dev/mapper/disk2 /mnt/storage 2>&1")` and throws away the returned string -- the `2>&1` is already there but unused -- while `cli/src/pool.rs#device_remove_error` keys the entire min-devices recovery hint on `stderr.contains("unable to go below")`, wording that originates in vendored `reference/btrfs-progs/common/utils.h` (`btrfs_err_str`, printed by `cmds/device.c`), i.e. text braid does not control. `rg 'go below' tests/ docs/` returns zero hits; every occurrence outside `reference/` is either the classifier itself or a hand-fed mock/fixture (`cli/src/pool.rs` unit tests, `cli/src/remove_missing.rs`, `cli/src/test_fixtures/shared.rs`, whose own comment concedes it only pins the builders, not live output). `docs/dev/testing.md#live-tool-behavior-locks` mandates exactly this lock for `stderr.contains(...)` classifiers, and the sibling classifier in the same file (`replace_error`'s `"scrub is in progress"`) already has one -- `tests/repro/btrfs-replace-rejected-during-scrub.py` asserts `re.search(r"scrub is in progress", output, re.IGNORECASE)` -- so the fix is internal-consistency, not new taste. The one deliberate-decision counterargument (`plans/impl/2026-05-11-device-remove-min-devices-hints.md` says "No VM test needed") does not survive: its rationale covers the classifier's routing logic being unit-testable, not the upstream wording it depends on, which is precisely the gap the doctrine names.

#### TASK-18: `cli/tests/golden_nixos_26_05.rs:11` [medium/small]

*Lens: test-gaps | category: weak-gate*

**Claim:** The authoritative stable golden lane skips silently on a missing fixture while the explicitly non-authoritative unstable lane hard-fails, inverting the intended strictness.

**Before:**

~~~~
//! (via `just capture-fixtures`) and verify the parsers handle it correctly.
//! If fixtures haven't been captured yet, tests are skipped.
...
const REQUIRE_FIXTURES: bool = false;

Per cli/tests/support/golden_common.rs:9-10, `false` means a missing fixture prints `SKIP:` and returns; `true` panics. cli/tests/golden_nixos_unstable.rs:11 sets `true`.
~~~~

**After:**

~~~~
const REQUIRE_FIXTURES: bool = true;

and update the module doc line 5 to say the nixos-26.05 fixtures are committed and required. A deleted or renamed fixture then fails loudly instead of turning 27 golden tests green-by-skip.
~~~~

**Finder's evidence:** docs/dev/parser-compatibility.md: "Fixtures in `cli/tests/fixtures/nixos-26.05/` are committed and authoritative" -- and the unstable fixtures, described as "non-authoritative", are the ones that hard-fail. `git ls-files cli/tests/fixtures/ | wc -l` = 83; all fixtures are tracked, and I verified every fixture name referenced by golden_common.rs resolves in both nixos-26.05/ and nixos-unstable/ (including the `upsc/` subdir), so flipping the flag is safe today. The skip semantics are a leftover from when fixtures were uncaptured, which the module doc comment still describes. The `smartctl-nvme-healthy.json` finding above is a concrete instance of this class of silent-skip going unnoticed.

**Verifier's correction:** Count detail is off: 31 of the 32 stable golden tests are skip-capable (only `golden_smartctl_sata_with_temperature` already panics), not 27. Also, the fix should update two more stale comments for accuracy -- the `golden_common.rs` header ("When false, missing fixtures skip the test (stable lane)") and the `REQUIRE_FIXTURES = false` clause in the `device_usage_clamps_negative_unallocated` preamble in `cli/src/parse/btrfs_device_usage.rs`. Once both lanes are `true` the const and the `let Some(...) else { eprintln!("SKIP: ...") }` branches are vestigial, so the more complete fix under this repo's "reach for the ideal, simple solution" rule is to delete `REQUIRE_FIXTURES`, make `fixture()` return `String` and panic on missing, and drop the skip branches (including `upsc_fixture`/`upsc_ok`'s `Option` plumbing).

**Verifier's reasoning:** The "before" is exact: `/Users/dan/Code/braid/cli/tests/golden_nixos_26_05.rs` line 11 is `const REQUIRE_FIXTURES: bool = false;` with the stale module-doc line 5 ("If fixtures haven't been captured yet, tests are skipped"), while `cli/tests/golden_nixos_unstable.rs` line 11 sets `true`, and `cli/tests/support/golden_common.rs#fixture` panics only when the flag is true. The inversion is real against documented intent: `docs/dev/parser-compatibility.md` calls the nixos-26.05 fixtures "committed and authoritative" and advertises fail-on-missing only for the explicitly "non-authoritative" unstable lane; nothing in the ADRs, the fixture READMEs, or `docs/dev/testing.md` gives a rationale for stable leniency -- and the only in-tree commentary (`cli/src/parse/btrfs_device_usage.rs#device_usage_clamps_negative_unallocated`, "the stable lane skips it when absent ... would regress silently") treats it as a hazard needing a synthetic backstop, not a design. I ran `cargo test --test golden_nixos_26_05 -- --nocapture`: 32 passed, zero `SKIP:` lines, so every referenced fixture (including `upsc/`) resolves and flipping the flag is a behavioral no-op today; the stable capture recipes use `cp -f` with no `rm -rf` (unlike `capture-fixtures-unstable`), so there is no window where stable fixtures are legitimately absent. No convention in AGENTS.md is violated (this is test-harness code, no CLI output, no ADR touches golden-lane strictness).

#### TASK-19: `cli/src/lock.rs:684` [medium/small]

*Lens: test-gaps | category: missing-live-tool-lock*

**Claim:** The `exit_status == 2 && stderr.contains("Not running")` classifier for `btrfs balance pause` has no live-tool test asserting that code and wording.

**Before:**

~~~~
                    if pause_result.exit_status == 2 && stderr.contains("Not running") {
                        emit_status(&line(
                            StatusTag::Warn,
                            "pool: balance was no longer running -- continuing",
                        ));
                    } else {
                        return Err(LockError::Failed(format!(
                            "btrfs balance pause {mount_point} failed (exit {}): {stderr}",
~~~~

**After:**

~~~~
Add a deterministic behavior lock to an existing VM test with a mounted idle pool (e.g. `tests/cli/braid-lock.py`, which sets up a 3-disk pool at /mnt/storage): run `btrfs balance pause /mnt/storage` with no balance running, assert exit code 2 and that stderr contains `Not running`. No new VM fixture is needed.
~~~~

**Finder's evidence:** docs/dev/testing.md#live-tool-behavior-locks requires a live-tool assertion for exactly this classifier shape, and names `tests/repro/cryptsetup-close-mounted.py` (which locks cryptsetup exit 5 / exit 4) as the reference example. `rg -n 'Not running|balance pause' tests/` finds only `tests/capture-tool-fixtures.py:163` and `tests/module/balance_helpers.py:45`, both of which run `btrfs balance pause ... 2>/dev/null` in a retry loop and discard stderr. The wording comes from vendored `reference/btrfs-progs/cmds/balance.c:713` (`(errno == ENOTCONN) ? "Not running" : strerror(errno)`). Failure mode: a btrfs-progs rewording turns the benign "balance finished before we paused it" race into a hard `braid lock` refusal with the pool left mounted.

**Verifier's correction:** The gap is broader than "no live-tool test": the exit-2/"Not running" branch has no coverage of any kind -- no mocked unit test constructs that stderr either (the string exists nowhere in the repo outside cli/src/lock.rs:684 and vendored reference/btrfs-progs). A complete fix adds both the live-tool assertion in tests/cli/braid-lock.py and a mocked execute-path test for the warn-and-continue branch; alternatively, consider relaxing the classifier to exit-code-only (exit 2 is uniquely ENOTCONN in balance.c), mirroring how cryptsetup-close-mounted.py already demotes the wording check to non-load-bearing.

**Verifier's reasoning:** The "before" text is verbatim at cli/src/lock.rs:684 inside `LockPlan::execute` (the classifier fires only when `pause_balance_before_unmount` was set by `lock_preflight_pause_decision`, i.e. a balance was seen running, so the "finished before we could pause it" race is genuinely reachable). `rg -l "Not running"` over the whole repo excluding `reference/` returns exactly one file -- cli/src/lock.rs -- so no VM test, repro, fixture, or even mocked unit test asserts that code/wording; the only mocked `CmdRequest::BtrfsBalancePause` stubs in lock.rs (`mounted_systemd_stop_runner`, and the dry-run assertion) return exit 0, and the two tests/ hits (`tests/capture-tool-fixtures.py`, `tests/module/balance_helpers.py`) redirect stderr to /dev/null in a retry loop. docs/dev/testing.md#live-tool-behavior-locks mandates a live-tool assertion for exactly this `exit_code == N && stderr.contains(...)` shape and names `tests/repro/cryptsetup-close-mounted.py` as the pattern; the plan that introduced this code (plans/impl/2026-05-30-systemd-stop-balance-pause.md) never lists such a gate, so this is an oversight rather than a deliberate exemption. The wording/code pair is confirmed upstream at reference/btrfs-progs/cmds/balance.c (`(errno == ENOTCONN) ? "Not running"` with `ret = 2`), and tests/cli/braid-lock.nix already ships `pkgs.btrfs-progs` with a mounted 3-disk pool at /mnt/storage, so the proposed subtest needs no new fixture and breaks no ADR or convention (test files are exempt from the ASCII rule anyway, and this adds no CLI output).

#### TASK-20: `justfile:348` [medium/small]

*Lens: test-gaps | category: unguarded-guard*

**Claim:** `check-see-paths.py` is the only docs guard with no `--selftest`, so nothing proves it can still fail; a regex or path-resolution regression would make it pass vacuously.

**Before:**

~~~~
check-docs-see-paths:
    python3 scripts/docs/check-see-paths.py

Every sibling recipe runs the selftest first, e.g. justfile:352-354 `check-output-ascii: python3 .../check-output-ascii.py --selftest` then the real run. `rg -c selftest scripts/docs/*.py` shows check-cmd-doc-comments(5), check-code-doc-anchors(5), check-doc-links(8), check-doctor-table-parity(7), check-line-cites(5), check-output-ascii(6), check-plans-refs(5) -- and check-see-paths(0).
~~~~

**After:**

~~~~
Add a `--selftest` mode to scripts/docs/check-see-paths.py that feeds it a synthetic `## See` section with a known-bad code-span path and asserts a non-zero result, then run it first in the recipe:

check-docs-see-paths:
    python3 scripts/docs/check-see-paths.py --selftest
    python3 scripts/docs/check-see-paths.py
~~~~

**Finder's evidence:** AGENTS.md names this script as the enforcement mechanism for the frozen-ADR / `## See` citation rule ("enforced by `scripts/docs/check-see-paths.py`"), so it guards a convention with no other backstop. I ran all nine checkers directly; all pass today, so the gap is latent rather than active. .github/workflows/checks.yml:13-19 runs the bare script while its seven sibling jobs (lines 21-91) all run selftest-first, making this the single unverified guard in the CI set.

**Verifier's correction:** Two details are overstated. (1) It is not the only docs script lacking `--selftest` -- check-doc-tables.py, check-frontmatter.py, and check-rendered-frontmatter.py also lack one; the accurate framing is that it is the only one of the eight CI guard jobs in .github/workflows/checks.yml that runs bare, and the only holdout among the citation/convention lints that adopted the mandatory-selftest norm. (2) "Nothing proves it can still fail" reads stronger than reality: the guard demonstrably fails on an unresolved path today and exercises 77 live targets, so the value is durable regression coverage for the regex/section-scoping subtleties, not a currently-broken check. The fix should also update .github/workflows/checks.yml:19 to run `--selftest` first, and will require parameterizing the module-level `ROOT`/`DECISIONS` globals (or building the fixture under a temp root) since `validate_bullet` calls `path.relative_to(ROOT)`.

**Verifier's reasoning:** Verified the "before": justfile:347-348 is `check-docs-see-paths:` / `python3 scripts/docs/check-see-paths.py` with no selftest, and .github/workflows/checks.yml:13-19 runs it bare while the other seven CI guard jobs all run `--selftest` first; `rg -c selftest scripts/docs/*.py` confirms check-see-paths.py is the only one of those eight with zero selftest code. This is historical drift, not deliberate: `git log` shows check-see-paths.py landed 2026-06-02 (13be2359), and the selftest norm arrived two days later with the ASCII guard, whose plan (plans/impl/2026-06-04-cli-output-ascii-guard.md:129-131) states "Mandatory `--selftest` ... the lexical logic needs durable regression coverage, so `--selftest` is **required** and runs **first**" while explicitly modeling itself on check-see-paths.py -- so the fix follows house convention rather than fighting it. I also confirmed the guard is not vacuous today (it validates 77 code-span targets across 21 ADRs, and a temp-tree reproduction exits 1 on an unresolved path), so the gap is regression protection for untested subtleties -- the exact `## See` heading match, the ` -- `/em-dash description split, the `#anchor` and `:NN` stripping, and the "preserved in git history" exemption -- all of which could silently narrow coverage to zero without any signal.

### Medium value, medium effort

#### TASK-21: `cli/src/confirm.rs:147` [medium/medium]

*Lens: perf | category: subprocess-waste*

**Claim:** `query_disk_hw_info` spawns three separate `lsblk` processes per disk to read MODEL, SERIAL and SIZE, even though the repo already has a fixture-covered `lsblk --json` parser that returns all three fields from one invocation.

**Before:**

~~~~
```rust
pub fn query_disk_hw_info<R: CommandRunner>(runner: &R, device: &str) -> DiskHwInfo {
    let model = get_lsblk_field(runner, device, LsblkFieldKind::Model);
    let serial = get_lsblk_field(runner, device, LsblkFieldKind::Serial);
    let size_str = get_lsblk_field(runner, device, LsblkFieldKind::Size);
```
Each `get_lsblk_field` call runs `CmdRequest::LsblkField`, which `cli/src/cmd.rs` renders as a full process spawn: `lsblk -n -d -b -o <MODEL|SERIAL|SIZE> <device>`. `cli/src/status.rs` repeats the pattern inside the per-present-disk loop:
```rust
let model = get_lsblk_field(runner, &pd.underlying, LsblkFieldKind::Model);
let serial = get_lsblk_field(runner, &pd.underlying, LsblkFieldKind::Serial);
```
So a 4-disk `braid add` spawns 12 lsblk processes and a 4-disk `braid status` spawns 8, all of which are the same query against the same devices.
~~~~

**After:**

~~~~
Add a device-scoped JSON request (e.g. `CmdRequest::LsblkDeviceJson { device }` rendering `lsblk --json --bytes --output NAME,TYPE,SIZE,MODEL,SERIAL <device>`, or an optional `device` on the existing `LsblkJson`), parse it with the existing `parse_lsblk_json`, and read `LsblkDevice { size, model, serial }` off the single returned blockdevice. `query_disk_hw_info` becomes one spawn instead of three; `status.rs`'s per-disk pair becomes one instead of two. `LsblkFieldKind` and `CmdRequest::LsblkField` can then be deleted.
~~~~

**Finder's evidence:** Verified in the current tree: `cli/src/cmd.rs#CmdRequest::LsblkField` (arg builder at cmd.rs:654-671) emits one process per field. The alternative already exists and is already used: `cli/src/cmd.rs#CmdRequest::LsblkJson` (cmd.rs:526-534) runs `lsblk --json --bytes --output NAME,TYPE,SIZE,MODEL,SERIAL,UUID,ROTA,TRAN`; `cli/src/parse/types.rs#LsblkDevice` (types.rs:11-21) already carries `size: Option<u64>`, `model: Option<String>`, `serial: Option<String>`; `cli/src/parse/lsblk.rs#parse_lsblk_json` (lsblk.rs:63) is the parser, exercised today by `cli/src/tui/probe.rs` (probe.rs:328). Call sites of the wasteful helper: `cli/src/confirm.rs#query_disk_hw_info` (confirm.rs:147-149), `cli/src/status.rs#build_disk_views` (status.rs:1155-1156, inside the per-present-device loop), reached from `cli/src/add.rs:1086`, `cli/src/remove.rs:268`, `cli/src/replace.rs:457-458`, `cli/src/preflight.rs:554`. Secondary support: `docs/design/decisions/010-toolchain-pinning.md` justifies leaving util-linux unpinned specifically because "braid consumes `lsblk --json` through tolerant serde" (line 10, and the table row at line 49); the plain-text `lsblk -n -d -b -o <FIELD>` form is outside that stated contract, so the change also narrows the parsed surface to the one the ADR blesses. Caveat: today each field degrades to `None` independently; after the change all three degrade together — in practice identical, since they are the same command against the same device. Tests that seed `CmdRequest::LsblkField` in mock runners (add.rs, remove.rs, replace.rs, status.rs, confirm.rs) would need reseeding, which is why this is medium effort.

**Verifier's correction:** The core claim survives; three refinements. (1) The headline benefit is contract narrowing and dedup, not user-visible latency -- both hot paths already spawn a per-disk `smartctl -H` (status.rs:1179) that dominates the saved lsblk spawns, so the wall-clock win is small. (2) The candidate missed the sharpest instance: `cli/src/preflight.rs#target_raw_size` calls `query_disk_hw_info` purely to read `.size`, i.e. three spawns for one needed field. (3) "A 4-disk `braid add` spawns 12" means adding four disks in one invocation (`work_plan.prelude.confirm_disks`), not operating on a 4-disk pool; the 4-disk `status` figure of 8 is correct.

**Verifier's reasoning:** The "before" is verbatim accurate: `cli/src/confirm.rs#query_disk_hw_info` (lines 146-150) makes three `get_lsblk_field` calls, each issuing `CmdRequest::LsblkField`, which `cli/src/cmd.rs#CmdRequest::to_argv` (lines 654-671) renders as `lsblk -n -d -b -o <MODEL|SERIAL|SIZE> <device>`; `RealRunner::run` has no caching layer, so each is a real spawn, and `cli/src/status.rs#build_disk_views` (1155-1156) repeats two of them inside the per-present-device loop. The one-shot alternative is already in-tree and fixture-covered: `CmdRequest::LsblkJson` (cmd.rs:526-534) plus `parse_lsblk_json`, whose `LsblkDevice` already carries `size`/`model`/`serial`, and `cli/tests/fixtures/*/lsblk-2disk.json` is the only captured util-linux fixture -- the plain-text single-column form is not fixture-covered at all. That matters beyond perf: `docs/design/principles.md` (Principle 6) and ADR 010 justify leaving util-linux host-provided *only* for "tolerant structured JSON, fail-closed on missing requested keys, and fixture-covered", and the `-o <COLUMN>` text form is none of those (a dropped column exits non-zero and silently degrades to `None`). I found no ADR, doc, comment, or git-history rationale defending the per-field form; `CmdRequest` is matched exhaustively in exactly one place (`to_argv`), and the one safety test that inspects argv (`replace.rs:6636-6645`, forbidding decoy by-id paths in any request's argv) still holds because the device stays in argv.

### Low value, trivial effort

#### TASK-22: `cli/src/btrfs_ioctl.rs:117` [low/trivial]

*Lens: dead-dup | category: dead-code*

**Claim:** `PanicBtrfsDevInfo` is test-support scaffolding with zero call sites anywhere in the repo; its `#[allow(dead_code)]` exists solely to silence the warning that would otherwise report it.

**Before:**

~~~~
    #[allow(dead_code)]
    pub(crate) struct PanicBtrfsDevInfo;

    impl BtrfsDevInfo for PanicBtrfsDevInfo {
        fn total_bytes(&self, mount: &Path, devid: Devid) -> Result<u64, BtrfsIoctlError> {
            panic!(
                "planner-boundary test: BtrfsDevInfo must not be invoked; got mount={} devid={devid}",
                mount.display()
            );
        }
    }
~~~~

**After:**

~~~~
Delete the struct, its `impl BtrfsDevInfo`, and the `#[allow(dead_code)]` from `tests_support`. (If the planner-boundary assertion it was meant to provide is still wanted, add a test that actually uses it -- but nothing does today.)
~~~~

**Finder's evidence:** `rg -n -w PanicBtrfsDevInfo` over the whole repo returns only the definition and its impl in `cli/src/btrfs_ioctl.rs`, plus one prose mention in `plans/impl/2026-05-23-replace-target-size-preflight.md` (a transient plan file, out of scope per AGENTS.md doc-citation rules). The sibling `MockBtrfsDevInfo` in the same `tests_support` module is used and carries no allow, confirming the allow marks this one item as unreached scaffolding.

**Verifier's correction:** The claim survives, with one nuance worth recording: the plan that introduced it intended `PanicBtrfsDevInfo` for planner-boundary tests, but the in-module test shims `plan_replace`/`cmd_replace` in `cli/src/replace.rs` inject `replace_dev_info_sufficient()` (a permissive `MockBtrfsDevInfo`) even in the `&PanicRunner`/`&PanicFilesystem` boundary tests, so the intent was never wired up. Deleting is correct today; the equally valid alternative is to plumb a dev_info parameter through those shims and pass `PanicBtrfsDevInfo` in the abort-before-any-probe tests.

**Verifier's reasoning:** I read cli/src/btrfs_ioctl.rs lines 84-127: the "before" snippet matches the current source exactly (`#[allow(dead_code)]` at 116, `pub(crate) struct PanicBtrfsDevInfo;` at 117, impl at 119), inside a `#[cfg(test)] pub(crate) mod tests_support`. A whole-repo `rg` for `PanicBtrfsDevInfo` returns only the definition, its impl, and one prose line in the already-implemented plan `plans/impl/2026-05-23-replace-target-size-preflight.md`; the sibling `MockBtrfsDevInfo` is imported by cli/src/preflight.rs, cli/src/replace.rs, and cli/src/test_fixtures/replace.rs and carries no allow, so the allow is marking exactly this unreached item. Nothing in AGENTS.md, docs/design/principles.md, or the ADRs blesses keeping unused test scaffolding, and the deliberate-keep convention that does exist (the "KEEP &PanicRunner / &PanicFilesystem" comment at cli/src/replace.rs) is absent here; since the code is `#[cfg(test)]`-only and referenced nowhere, deletion compiles and changes no behavior or test.

**Implemented:** Kept the purpose-built sentinel, passed it directly to every replace test whose contract forbids all injected probes, and removed the dead-code allowance. The ordinary replace-test shims continue to use the permissive `MockBtrfsDevInfo` fixture.

#### TASK-23: `modules/braid/options.nix:119` [low/trivial]

*Lens: dead-dup | category: unreachable-branch*

**Claim:** The `braid.autoUnlock.timeoutSec > 0` assertion can never fire because the option's type is already `lib.types.ints.positive`, which rejects any non-positive value during option type-checking.

**Before:**

~~~~
      {
        assertion = cfg.autoUnlock.enable -> cfg.autoUnlock.timeoutSec > 0;
        message = "braid.autoUnlock.timeoutSec must be positive.";
      }
~~~~

**After:**

~~~~
Delete the assertion entirely; the type already enforces it and produces a better message ("is not of type `positive integer, meaning >0'").
~~~~

**Finder's evidence:** `modules/braid/options.nix` declares `timeoutSec = lib.mkOption { type = lib.types.ints.positive; default = 5; ... }` (same file, ~line 65). nixpkgs' `types.ints.positive` is `addCheck types.int (x: x > 0)`, so a value <= 0 fails option merging before assertions are evaluated. `git show e80a71e1 -- modules/braid/options.nix` shows the type and the assertion were introduced together, so the assertion has been dead since day one. `rg -n "timeoutSec must be positive"` finds no test or doc pinning the message.

**Verifier's correction:** Minor detail: the option block starts at line 65 with `type = lib.types.ints.positive;` on line 66 (evidence said "~line 65"). More importantly, Nix evaluation is lazy: with auto-unlock disabled, an otherwise-unused `timeoutSec = 0` can remain unforced and the system still evaluates. With auto-unlock enabled, the option is forced and the type error wins before the assertion can produce its message. `docs/guides/nixos-configuration.md` already documents the type as "positive int", so deleting the assertion leaves the documented contract intact and needs no doc change.

**Verifier's reasoning:** I read `modules/braid/options.nix` and the "before" matches lines 118-121 exactly, with `timeoutSec` declared at lines 65-69 as `type = lib.types.ints.positive`. I confirmed against the actual nixpkgs source in the Nix store (`lib/types.nix`): `positive = addCheck lib.types.int (x: x > 0) // { name = "positiveInt"; description = "positive integer, meaning >0"; }`. A live NixOS evaluation with auto-unlock enabled and `timeoutSec = 0` fails with the type diagnostic (`is not of type 'positive integer, meaning >0'`) before the assertion can return false; with auto-unlock disabled, both the typed value and the assertion's consequent remain lazy, so the dormant invalid value evaluates successfully. Removing the assertion therefore preserves both behaviors. `git show e80a71e1` confirms the type and the assertion landed in the same commit, so its message has been unreachable since introduction; `rg` over `tests/` (including the dedicated `tests/eval/*-assertion-*.nix` harness) and `docs/` finds nothing pinning the message, and no ADR or principle enumerates it (only the archived plan `plans/impl/2026-01-01-predated/auto-unlock2.md` lists it, which is a historical record, not authority). The other assertions in the same list are genuinely non-redundant (mountPoint grammar, group regex, deadline-vs-constant, by-id prefix), so this is an outlier rather than a deliberate defense-in-depth pattern.

**Implemented:** Deleted the unreachable assertion. The positive integer option type remains the sole validation contract, with no test, guide, README, principle, or ADR changes required.

#### TASK-24: `cli/src/replace.rs:390` [low/trivial]

*Lens: dead-dup | category: duplicated-logic*

**Claim:** The RAID1 soft-balance `Step` (risk tag, dry-run description, and command) is constructed twice with identical text, even though the decision to emit it is already single-sourced through `pool::should_restore_raid1`.

**Before:**

~~~~
cli/src/replace.rs:390-400 and cli/src/remove_missing.rs:124-134 both contain:
        if self.restore_raid1_after_commit {
            steps.push(Step {
                risk: "long",
                description:
                    "btrfs balance -dconvert=raid1,soft -mconvert=raid1,soft (restore redundancy)"
                        .into(),
                commands: vec![CmdRequest::BtrfsBalanceRaid1Soft { mount_point: ... }],
            });
        }
~~~~

**After:**

~~~~
Add one builder next to the shared predicate, e.g. `pub(crate) fn raid1_soft_balance_step(mount_point: &MountPoint) -> Step` in `cli/src/pool.rs` (beside `should_restore_raid1`), and have both plans do `steps.push(pool::raid1_soft_balance_step(mp))`.
~~~~

**Finder's evidence:** Both call sites already share the gate: `cli/src/remove_missing.rs:486` and `:725` use `crate::pool::should_restore_raid1(...)`, and `cli/src/replace.rs` sets `restore_raid1_after_commit` the same way. Principle 3 in `docs/design/principles.md` states both `remove-missing` and `replace` (missing path) "run a follow-up soft balance to restore RAID1 profiles" -- one behavior, so one step builder. The `description` string is operator-visible dry-run output pinned by `--dry-run` tests, so duplicating it is exactly the drift risk ADR 022's "rendered from the same typed work plans" rule exists to prevent.

**Verifier's correction:** The duplication is real and worth collapsing, but three details in the pitch are overstated. (1) Drift risk is confined to the operator-facing description/risk label -- the `$ btrfs balance start --enqueue ...` line both previews print already comes from the shared `CmdRequest::BtrfsBalanceRaid1Soft::to_argv`, so no command can diverge. (2) There is a third render site for the same operation, `cli/src/recover.rs#render_recovery_tail` (near line 944), which deliberately uses different text (includes the mount point and "(skipped if pool has <2 devices)", with `commands: vec![]` because replay is conditional); a shared builder would cover 2 of 3 sites, not "one behavior, one builder". (3) ADR 022's "rendered from the same typed work plans" rule is about preview and execution sharing one typed plan, not about two commands sharing prose, so it does not mandate this change -- the justification is ordinary DRY plus the existing helper precedent. Also, `Step` lives in `cli/src/cmd.rs` (which already hosts the ADR-022 shared-render precedent `luks_format_argv`) and `cli/src/pool.rs` currently constructs no `Step`s, so cmd.rs is at least as defensible a home as pool.rs; whichever is chosen, the new `pub(crate)` fn needs a `///` per the doc-comment convention.

**Verifier's reasoning:** I read both cited ranges: `cli/src/replace.rs:390-400` and `cli/src/remove_missing.rs:124-134` are byte-identical Step constructions (risk "long", the same "btrfs balance -dconvert=raid1,soft -mconvert=raid1,soft (restore redundancy)" literal, `CmdRequest::BtrfsBalanceRaid1Soft` with the respective mount point), and the gate really is single-sourced -- `cli/src/pool.rs#should_restore_raid1` is used at remove_missing.rs:486, :725, :747 and replace.rs:1747, and its own doc comment claims to be the "Single source for the `restore_raid1_after_commit` journal flag, the dry-run preview step, and the operator confirmation, so all three always agree". Both sites were introduced in one commit (443e1e39) with no doc, ADR, or comment sanctioning the duplication, and the repo already factors repeated Step construction into helpers (`cli/src/add.rs#push_returned_disk_enrollment_steps`, `cli/src/add.rs#forced_returned_device_add_step`), so the fix matches house style; tests only assert substrings ("-dconvert=raid1,soft", "restore redundancy"), so an identical-text extraction breaks nothing. It is genuine but low-stakes: the executed/rendered argv is already single-sourced through `CmdRequest::BtrfsBalanceRaid1Soft::to_argv`, so only the human-readable description line can drift.

#### TASK-25: `cli/src/mount.rs:359` [low/trivial]

*Lens: control-flow | category: duplication*

**Claim:** `compile_open_steps` duplicates the whole `Step` literal across the keyfile/passphrase branches when only the single `CmdRequest` inside `commands` differs.

**Before:**

~~~~
```rust
    for (name, by_id) in &plan.to_unlock {
        let mn = mapper_name(name);
        if let Some(kf) = key_file {
            steps.push(Step {
                risk: "safe",
                description: format!("LUKS open {} -> {}", by_id, mn),
                commands: vec![CmdRequest::CryptsetupLuksOpenKeyFile {
                    device: by_id.as_str().to_owned(),
                    mapper: mn.clone(),
                    key_file_path: kf.display().to_string(),
                }],
            });
        } else {
            steps.push(Step {
                risk: "safe",
                description: format!("LUKS open {} -> {}", by_id, mn),
                commands: vec![CmdRequest::CryptsetupLuksOpen {
                    device: by_id.as_str().to_owned(),
                    mapper: mn.clone(),
                }],
            });
        }
    }
```
~~~~

**After:**

~~~~
```rust
    for (name, by_id) in &plan.to_unlock {
        let mn = mapper_name(name);
        let open = match key_file {
            Some(kf) => CmdRequest::CryptsetupLuksOpenKeyFile {
                device: by_id.as_str().to_owned(),
                mapper: mn.clone(),
                key_file_path: kf.display().to_string(),
            },
            None => CmdRequest::CryptsetupLuksOpen {
                device: by_id.as_str().to_owned(),
                mapper: mn.clone(),
            },
        };
        steps.push(Step {
            risk: "safe",
            description: format!("LUKS open {} -> {}", by_id, mn),
            commands: vec![open],
        });
    }
```
~~~~

**Finder's evidence:** `risk` and `description` are character-identical in both arms (`cli/src/mount.rs#compile_open_steps`, lines 360-378); only the `CmdRequest` variant differs. `mn` is still borrowed by the `description` format after the match, and both arms already `mn.clone()` into the request, so ownership is unchanged. `Step` is output-only per principle 3 / ADR 022, so the rendered dry-run bytes are unaffected.

**Verifier's correction:** Core claim holds as stated. Minor refinement: the `after` can be slightly better still -- build `description` before the `match` and move `mn` into the request, dropping the remaining `.clone()` in both arms.

**Verifier's reasoning:** I read `cli/src/mount.rs#compile_open_steps` (lines 350-379) and the "before" snippet is character-exact with the current source: both arms build a `Step` with identical `risk: "safe"` and identical `description: format!("LUKS open {} -> {}", by_id, mn)`, differing only in the single `CmdRequest` variant. The hoist is behavior-preserving: `key_file` is `Option<&Path>` (Copy, so the `match` borrows nothing extra), both arms already `mn.clone()` into the request so `mn` remains available for the `format!`, and `Step { risk, description, commands }` (`cli/src/cmd.rs#Step`) renders purely from those fields, so `Step::render_dry_run` output bytes are unchanged -- the preview-ordering assertions in `cli/src/unlock.rs#plan_unlock_dry_run_render_2_closed_disks_with_key_file` and the mount.rs keyfile tests still pass. ADR 022 only requires that `Step` stay output-only and not be cached in plan structs; nothing in it, principles.md, or safety-heuristics.md prescribes per-branch literals, and no AGENTS.md convention (ASCII output, error prefixes, doc comments) is touched. This is genuine mechanical duplication, but it is small and low-risk, so the payoff is modest.

**Implemented:** Built the shared open-step description once, selected only the credential-specific command in the branch, and moved the mapper name into that command. Existing passphrase and keyfile preview tests continue to cover both rendered forms.

#### TASK-26: `cli/src/tui/browse/state.rs:728` [low/trivial]

*Lens: control-flow | category: control-flow*

**Claim:** `command_finished` opens two consecutive `if self.mode == BrowseMode::Normal` blocks with no mutation of `self.mode` between them.

**Before:**

~~~~
```rust
        if self.mode == BrowseMode::Normal {
            self.subvolumes.clear();
            self.systemd_units.clear();
            if self.is_subvolume_list() {
                ...
            } else if self.is_systemd_picker() {
                ...
            }
        }

        if self.mode == BrowseMode::Normal {
            self.cache.insert(
                self.current_selection(),
                CachedOutput { ... },
            );
        }
```
~~~~

**After:**

~~~~
Fold the cache insert into the first block's tail so the mode is tested once:
```rust
        if self.mode == BrowseMode::Normal {
            self.subvolumes.clear();
            self.systemd_units.clear();
            if self.is_subvolume_list() {
                ...
            } else if self.is_systemd_picker() {
                ...
            }
            self.cache.insert(
                self.current_selection(),
                CachedOutput { ... },
            );
        }
```
~~~~

**Finder's evidence:** Between the two tests (state.rs:702-726 and 728-737) the only writes are to `self.subvolumes`, `self.systemd_units`, `self.subvol_selected`, and `self.systemd_unit_selected`; `self.mode` is never assigned in `command_finished` (the only assignments to `mode` are in `load_current`, `enter`, and `back`). The cached `CachedOutput` snapshot must still be taken after the parse block populates `subvolumes` / `systemd_units`, and appending it at the end of the first block preserves that ordering exactly.

**Verifier's correction:** Worth noting the guard is even more redundant than claimed for the parse branches: `is_subvolume_list()` and `is_systemd_picker()` each already re-test `self.mode == BrowseMode::Normal` internally (state.rs:839, 855). The outer guard is still required, though, for the unconditional `subvolumes.clear()` / `systemd_units.clear()` and the cache insert.

**Verifier's reasoning:** The "before" snippet matches `cli/src/tui/browse/state.rs#BrowseState::command_finished` exactly: line 702 opens the parse block and line 728 opens a second, identical `if self.mode == BrowseMode::Normal` for the cache insert. Everything executed between them is `&self` (`is_subvolume_list`, `is_systemd_picker`, `current_selection`) or free functions (`parse_btrfs_subvolume_list`, `parse_systemctl_list_units_json`), and `rg` shows the only writes to `self.mode` in the file are at lines 492, 618, 627, 633, 649 -- all in `load_current`/`enter`/`back`, never in `command_finished` -- so the second test can never differ from the first. `git log -L` proves this is refactoring residue rather than deliberate: in commit 282bcf20 the first guard was `self.current_selection() == BrowseSelection::BtrfsSubvolumes && self.mode == BrowseMode::Normal` (genuinely a different condition from the second) and was rewritten to the bare mode test without merging the blocks. Nothing in AGENTS.md, principles.md, or the ADRs governs this; the fold preserves ordering (cache still snapshots after parse) and is exactly behavior-preserving, so no test or doc is implicated.

#### TASK-27: `docs/design/decisions/020-ups-integration.md:55` [low/trivial]

*Lens: doc-drift | category: doc-drift*

**Claim:** Active ADR 020 names the NUT pin option `braid.packages.networkupstools`, but the module implements it as `braid.packages.nut`.

**Before:**

~~~~
Pinning is load-bearing. A new `braid.packages.networkupstools` option is added alongside the existing `btrfsProgs`, `cryptsetup`, and `utilLinux` pins (util-linux has since been un-pinned; see [decision 010](010-toolchain-pinning.md)), defaulted to nixos-26.05's `networkupstools`.
~~~~

**After:**

~~~~
Pinning is load-bearing. A new `braid.packages.nut` option is added alongside the existing `btrfsProgs`, `cryptsetup`, and `utilLinux` pins (util-linux has since been un-pinned; see [decision 010](010-toolchain-pinning.md)), defaulted to nixos-26.05's `nut`.
~~~~

**Finder's evidence:** `modules/braid/options.nix` line 26 defines `nut = lib.mkPackageOption pkgs "nut" { };` inside `options.braid.packages`; `modules/braid/ups.nix` line 105 consumes it as `package = cfg.packages.nut;`. No `networkupstools` option exists anywhere in `modules/`. Every other doc already uses the real name: docs/guides/nixos-configuration.md:"`braid.packages.nut` | package | `pkgs.nut`", docs/design/principles.md#10-pinned-toolchain lists `braid.packages.{btrfsProgs,cryptsetup,utilLinux,nut,smartmontools,ethtool}`, and AGENTS.md repeats that set as the fixture-refresh trigger. ADR 020 carries `status: Active`, so docs/dev/doc-citations.md#decision-doc-references does not freeze it -- only Superseded/Deprecated bodies are point-in-time records. The name is load-bearing: an operator following the ADR to override the pin would get an "option does not exist" eval error.

**Verifier's correction:** The fix is correctly scoped to line 55's option name. The other `networkupstools` occurrences in this ADR (line 53 "NUT (`networkupstools`)" and line 146 "nixpkgs bumps touching `networkupstools`") should be left alone -- they name the upstream project / nixpkgs derivation, matching the identical phrasing already used in Active ADR 010 ("A nixpkgs bump that touches `networkupstools`") and `scripts/fetch-references.py`'s github.com/networkupstools/nut clone. Only the `braid.packages.*` option name is wrong.

**Verifier's reasoning:** Line 55 of `docs/design/decisions/020-ups-integration.md` matches the quoted "before" verbatim, including "A new `braid.packages.networkupstools` option". The module contradicts it: `modules/braid/options.nix` defines `nut = lib.mkPackageOption pkgs "nut" { };` under `options.braid.packages`, consumed as `cfg.packages.nut` in `modules/braid/ups.nix` and `modules/braid/wrapper.nix`, and `rg networkupstools modules/` returns nothing -- so the ADR names an option that would fail `nix eval` if an operator copied it. Every other authority surface already uses the real name (`docs/design/decisions/010-toolchain-pinning.md` "Yes (`braid.packages.nut`)", `docs/design/principles.md#10-pinned-toolchain`, `docs/guides/nixos-configuration.md` table, AGENTS.md), so ADR 020 contradicts a peer Active ADR rather than merely wording things differently. ADR 020 carries `status: Active`, and `docs/dev/doc-citations.md#decision-doc-references` freezes only Superseded/Deprecated bodies; the same sentence already carries a post-hoc "util-linux has since been un-pinned" update and an explicit "exact nixpkgs option name to confirm during implementation" caveat, confirming this paragraph is meant to be kept current, not preserved as a point-in-time record.

#### TASK-28: `cli/src/main.rs:324` [low/trivial]

*Lens: doc-drift | category: doc-drift*

**Claim:** `braid add --enroll`'s help says the keyfile is enrolled in "the new disk" (singular), but `add` accepts multiple disk specs and enrolls slot 1 on every adopted disk.

**Before:**

~~~~
    /// Directory containing braid.key to enroll in the new disk (LUKS slot 1)
    #[arg(long = "enroll")]
    enroll_key_file: Option<std::path::PathBuf>,
~~~~

**After:**

~~~~
    /// Directory containing braid.key to enroll (LUKS slot 1) on each adopted disk
    #[arg(long = "enroll")]
    enroll_key_file: Option<std::path::PathBuf>,
~~~~

**Finder's evidence:** `AddArgs.disks` is `#[arg(required = true, num_args(1..))] disks: Vec<String>` (main.rs:322-323), and `cli/src/add.rs` threads `enroll_key_file` per target through the add loop (`if let Some(kf) = &target.enroll_key_file` at lines 701/735/764/1377, each emitting `disk {name}: enrolling keyfile in slot 1...`). docs/commands/add.md:73 already states "into LUKS slot 1 on each adopted disk -- fresh or returning" and add.md:97 elaborates "braid enrolls `braid.key` into LUKS slot 1 on every adopted disk". The identical wording at main.rs:364 is correct there because `ReplaceArgs` has exactly one `--new` target, so only the `AddArgs` copy needs to change.

**Verifier's correction:** The drift is slightly worse than stated: besides the singular "the new disk", the word "new" is itself inaccurate, since `add --enroll` also enrolls slot 1 on *returning* braid disks whose slot 1 is empty (docs/commands/add.md:97). The proposed "each adopted disk" wording happens to fix both, and matches the terminology already used in add.md.

**Verifier's reasoning:** I read `cli/src/main.rs` lines 319-373 and the "before" text matches byte-for-byte at line 324 inside `AddArgs`, where `disks: Vec<String>` is `#[arg(required = true, num_args(1..))]`, so `add` genuinely takes N disks. `cli/src/add.rs` builds targets in a `for (i, p) in input.probed.iter().enumerate()` loop and copies `input.enroll_key_file` onto every target (add.rs:2152, 2225, 2285), with per-target execution at 701/735/764/1377 emitting `disk {name}: enrolling keyfile in slot 1...`, so the keyfile really is enrolled on each adopted disk; `docs/commands/add.md:73,97` already say "each/every adopted disk -- fresh or returning". The identical string at main.rs:364 is correct for `ReplaceArgs` (single `new: String`), matching `docs/commands/replace.md:76` ("on the new disk"), so scoping the edit to the `AddArgs` copy is right. I grepped for the string repo-wide and for help snapshots (insta `.snap`, trycmd) and found no test or generated doc asserting this help text, and no ADR or principle governs the wording; the replacement is pure ASCII so `check-output-ascii.py` is unaffected.

#### TASK-29: `cli/src/mount.rs:332` [low/trivial]

*Lens: api-naming | category: doc-drift*

**Claim:** `print_probe_events`'s doc claims it is a thin wrapper around `render_probe_events`, but it does not call it -- it duplicates the body with a different color argument, so the two produce different bytes, and the "byte-for-byte stable" test pins the twin no production path uses.

**Before:**

~~~~
```
/// Thin stderr wrapper around `render_probe_events`. Callers invoke
/// this after `plan_open_pool` but before propagating any error, so
/// per-disk context always precedes a failure message.
pub fn print_probe_events(events: &[ProbeEvent]) {
    let notes: Vec<PreviewNote> = events.iter().map(ProbeEvent::to_preview_note).collect();
    let text = preview::render_notes_for_stderr_with(
        &notes,
        PerDiskStyle::Bracketed,
        color_enabled_for_stderr(),
    );
```
while `render_probe_events` (line 327) calls `preview::render_notes_for_stderr(&notes, PerDiskStyle::Bracketed)`, i.e. `color_enabled = false`.
~~~~

**After:**

~~~~
Adopt the house `X` / `X_with(color_enabled)` pair already used by `preview::render_notes_for_stderr`/`_with`, `Preview::render`/`render_with`, and `doctor::format_doctor_human`/`_with`: add `render_probe_events_with(events, color_enabled)`, define `render_probe_events(events) = render_probe_events_with(events, false)`, and have `print_probe_events` call `render_probe_events_with(events, color_enabled_for_stderr())`. The doc then becomes true and the byte-format test at mount.rs:1871 actually guards the production renderer.
~~~~

**Finder's evidence:** cli/src/mount.rs:327-345 (both bodies). `rg -n "render_probe_events"` shows its only call sites are mount.rs:1890 and mount.rs:1960, both inside `#[cfg(test)] mod tests` which starts at mount.rs:847-848; the production callers (cli/src/recover.rs:3531, cli/src/unlock.rs) use `print_probe_events`. preview.rs:150-151 and preview.rs:183-184 show the `_with` convention.

**Verifier's correction:** Only the doc sentence is defective. Correct claim: `print_probe_events`'s first doc line asserts it wraps `render_probe_events`, but since commit f8cf05c7 it inlines the note mapping and calls `preview::render_notes_for_stderr_with` with the stderr color policy instead. It is NOT true that "the two produce different bytes" in general (they are identical whenever color is off) nor that "the byte-for-byte stable test pins a twin no production path uses" (the pinned body is the production body; color wraps only the tag, and `tests/cli/unlock-uuid-mismatch.py` asserts the production row against it). The deliberate plain/colored split is prescribed by `plans/impl/2026-04-24-colorize-status-tags.md`. The right fix is to rewrite the doc sentence to state the real relationship and why `render_probe_events` stays uncolored (byte-pinnable); the proposed `render_probe_events_with` pair is an acceptable optional tidy that matches the `preview::render_notes_for_stderr`/`_with` house pattern and preserves the plan's intent, but it is not required and buys only two shared lines.

**Verifier's reasoning:** The "before" is accurate: `cli/src/mount.rs#print_probe_events` still carries the doc "Thin stderr wrapper around `render_probe_events`" while its body was rewritten by commit f8cf05c7 ("colorize status tags at render boundaries") to inline the note mapping and call `preview::render_notes_for_stderr_with(..., color_enabled_for_stderr())`; `git log -L 319,346:cli/src/mount.rs` shows the exact `-let text = render_probe_events(events);` deletion with the doc left untouched, so the sentence is stale in the strict sense. However the rest of the claim does not survive: the split is deliberate and recorded in `plans/impl/2026-04-24-colorize-status-tags.md` ("keep that delegation plain (byte-stable). `print_probe_events` calls `color_enabled_for_stderr()` and then `render_notes_for_stderr_with`"), and since `preview::render_notes_for_stderr(n, s) == render_notes_for_stderr_with(n, s, false)`, the two paths are byte-identical whenever stderr is not a TTY (all CI/VM runs), diverging only by an ANSI wrapper on the `[ok]`/`[skip]` tag -- which `preview.rs#render_notes_for_stderr_with_colors_bracketed_tags_only` covers. `tests/cli/unlock-uuid-mismatch.py` explicitly cross-checks the production stderr against the pinned body ("pinned by the Rust test render_probe_events_formats_mixed_probe_result; stderr is uncolored under capture, and color (when on) wraps only the [ok] tag, never the body"), so the byte test is not guarding an unused twin. `render_probe_events` is `pub` in a lib+bin crate (`cli/src/lib.rs` exists), so it is public API rather than dead code.

**Implemented:** Routed the remaining probe-event stderr wrapper through `preview::emit_notes_to_stderr`, corrected its stale boundary documentation, and added a color-enabled capture test of the production wrapper. The pure plain renderer remains byte-stable for existing format-contract tests.

#### TASK-30: `justfile:139` [low/trivial]

*Lens: api-naming | category: naming*

**Claim:** The `clippy-fix` recipe never runs clippy -- it runs `cargo fix`, which applies only rustc's machine-applicable suggestions, so `just clippy-fix` cannot fix anything `just clippy` reports.

**Before:**

~~~~
```
# Auto-fix compiler warnings in CLI tests where possible
clippy-fix:
    cargo fix --manifest-path cli/Cargo.toml --tests --allow-dirty
```
~~~~

**After:**

~~~~
Either rename the recipe to match what it does (`fix:`, keeping the accurate "Auto-fix compiler warnings" comment), or make it match its name: `cargo clippy --manifest-path cli/Cargo.toml --tests --fix --allow-dirty`. The name/effect pair as written sends anyone with a clippy failure to a recipe that is a no-op for their problem.
~~~~

**Finder's evidence:** justfile:135-140: `clippy:` runs `cargo clippy --manifest-path cli/Cargo.toml --tests`, `clippy-fix:` runs `cargo fix`. The two are different tools -- `cargo fix` does not load the clippy driver, so the project's own `[workspace.lints.clippy]` config in Cargo.toml (`result_large_err = "allow"`) and every clippy lint are outside its reach. AGENTS.md requires every justfile recipe to carry an explanatory comment; here the comment is accurate and the recipe name is not.

**Verifier's correction:** The recipe named `clippy-fix` runs `cargo fix`, which never loads the clippy driver and therefore cannot auto-apply any `clippy::` lint suggestion -- it only fixes the rustc-lint subset that `just clippy` happens to also surface, so the claim "cannot fix anything `just clippy` reports" is overstated. Of the two proposed fixes, the rename (`fix:`, keeping the already-accurate comment) is the safer one: `cargo clippy --fix` implies `--all-targets` (widening scope past `--tests`) and generally also wants `--allow-staged` alongside `--allow-dirty`.

**Verifier's reasoning:** I read `justfile` lines 134-140 and the "before" block is verbatim accurate: `clippy:` runs `cargo clippy --manifest-path cli/Cargo.toml --tests` while `clippy-fix:` runs `cargo fix ... --tests --allow-dirty`. `cargo fix` invokes plain rustc, not the clippy driver, so no `clippy::` lint suggestion (and nothing gated by `[workspace.lints.clippy]` in `Cargo.toml`) is reachable by it -- a real name/effect mismatch, and `cargo clippy --help` confirms `--fix` exists as the correct companion. Git history (`e31ed735 apply clippy fix`) shows the recipe was added in the same commit that added `--tests` to `clippy:`, with no ADR, doc, or principle sanctioning the naming; `rg` finds zero references to `clippy-fix` anywhere in docs, plans, scripts, or `.github`, so renaming breaks no caller or CI. Only the evidence's absolute claim is off: `cargo clippy` output also includes ordinary rustc warnings (unused imports/variables), which `cargo fix` *does* auto-apply, so `clippy-fix` is not a total no-op for a `just clippy` failure.

**Implemented:** Made `clippy-fix` run `cargo clippy --fix` and aligned both Clippy recipes on `--all-targets`, so checking and automatic fixes cover the same CLI target set. Kept `--allow-dirty`, which current Cargo documents as permitting both unstaged and staged changes.

#### TASK-31: `cli/src/lock.rs:1180` [low/trivial]

*Lens: api-naming | category: dead-parameter*

**Claim:** `cmd_lock_orchestrate_impl`'s `CL` closure bound carries a `bool` dry-run parameter that the function itself hardcodes to `false`, so the seam advertises a dry-run mode the orchestrator can never enter and every test closure has to name and ignore it.

**Before:**

~~~~
```
    CL: FnOnce(&R, &F, &Config, &PoolMembership, bool) -> Result<(), LockError>,
    MD: FnOnce() -> io::Result<()>,
{
    cmd_lock_fn(runner, fs, config, membership, false).map_err(LockOrchestrateError::CmdLock)?;
```
All three test closures bind it as `|_runner, _fs, _config, _membership, _dry_run|` (lock.rs:1512, 1548, 1584).
~~~~

**After:**

~~~~
Drop the trailing `bool` from the `CL` bound and from the call: `CL: FnOnce(&R, &F, &Config, &PoolMembership) -> Result<(), LockError>` / `cmd_lock_fn(runner, fs, config, membership)`. The production adapter at lock.rs:1160-1162 then reads `|runner, fs, config, membership| cmd_lock(runner, fs, config, membership, false, Vec::new())`, keeping the `false` at the one place that actually decides it.
~~~~

**Finder's evidence:** cli/src/lock.rs:1167-1187 (definition and the sole in-body call); call sites at lock.rs:1154 (production, via `cmd_lock_orchestrate`) and 1506/1542/1579 (tests). `cmd_lock_orchestrate` is the plain-lock ordering invariant path per its own doc at lock.rs:1140-1142; there is no dry-run orchestrate entry point -- `--dry-run` lock goes through `cmd_lock(..., dry_run: true, ...)` directly.

**Verifier's correction:** Two small adjustments. (1) The third test closure is at lock.rs:1585, not 1584 (the `cmd_lock_orchestrate_impl` call sites 1506/1542/1579 are correct). (2) A minor tradeoff the candidate does not mention: today the `false` decision sits inside the unit-tested `cmd_lock_orchestrate_impl`, and the "after" moves it into the untested production wrapper `cmd_lock_orchestrate`. That is still net-better -- with no `bool` in the seam at all, "plain lock is never a preview" becomes structural rather than a value nothing asserts -- but the change is API hygiene on a private test seam, not a correctness fix.

**Verifier's reasoning:** The "before" text matches `cli/src/lock.rs` verbatim: line 1180 is the `CL: FnOnce(&R, &F, &Config, &PoolMembership, bool)` bound and line 1183 calls `cmd_lock_fn(runner, fs, config, membership, false)`. `rg` shows `cmd_lock_orchestrate_impl` is private and has exactly four callers -- the production wrapper `cmd_lock_orchestrate` (lock.rs:1154, reached only from `run_plain_lock` at main.rs:1318) and three tests at 1506/1542/1579, all of which bind and ignore the flag (`|_runner, _fs, _config, _membership, _dry_run|`). Dry-run lock never touches the orchestrator: `run_dry_run_lock` (main.rs:1275-1286) calls `cmd_lock(..., true, extra_notes)` directly, so `true` is unreachable through this seam. It is not deliberate -- `git show 9aac1bb0` (the commit that introduced the orchestrator, with plan `plans/impl/2026-05-20-run-plain-lock-test-gap.md`) shows the bool was mirrored from `cmd_lock`'s arity, and the plan file never mentions `dry_run`; no doc, test, or Active ADR (022 governs planner/preview seams, not this closure) references it, and the fix is mechanical and behavior-preserving.

**Implemented:** Replaced the full argument-forwarding callback with a zero-argument injected lock operation. The production plain-lock wrapper now captures its command dependencies and owns the fixed non-preview choice, while the ordering tests no longer construct or ignore unrelated runner, filesystem, membership, or dry-run inputs.

#### TASK-32: `cli/src/main.rs:1198` [low/trivial]

*Lens: error-handling | category: error-context-loss*

**Claim:** `handle_pool_lock_error` prints the inner `io::Error` instead of the `PoolLockError`, discarding the variant's own `pool lock I/O error:` context, so a lock-file failure surfaces as a bare, unattributable errno.

**Before:**

~~~~
```rust
fn handle_pool_lock_error(error: PoolLockError) {
    match error {
        PoolLockError::AlreadyHeld | PoolLockError::DeadlineExpired { .. } => {
            eprintln!("{error}");
        }
        PoolLockError::Io(e) => print_cli_error(&e.to_string()),
    }
}
```
Because `PoolLockError::Io(e)` binds the inner error, `e.to_string()` renders only the errno text. `io_from_errno` (`cli/src/pool_lock.rs#io_from_errno`) builds the error with `io::Error::from_raw_os_error`, which carries no path either, so an EACCES/EROFS on `/run/braid-pool.lock` prints exactly `error: Permission denied (os error 13)`.
~~~~

**After:**

~~~~
Render the outer error so the declared prefix reaches the operator: `PoolLockError::Io(_) => print_cli_error(&error.to_string()),` (a `_` pattern does not move `error`, so this compiles unchanged). Output becomes `error: pool lock I/O error: Permission denied (os error 13)`.
~~~~

**Finder's evidence:** `cli/src/pool_lock.rs` declares `#[error("pool lock I/O error: {0}")] Io(#[from] io::Error)` at line 31. `rg -n 'pool lock I/O error' cli tests docs` returns only that declaration -- the string is unreachable in output today, because `handle_pool_lock_error` is the only consumer (called from `acquire_per_policy`, `acquire_pool_or_exit`, `acquire_pool_with_timeout_or_exit`, and `cli/src/main.rs:1372`). The sibling arm deliberately prints `{error}` (the outer value), confirming the Io arm is the outlier.

**Verifier's correction:** Two details are slightly off. (1) "the sibling arm ... confirming the Io arm is the outlier" overstates it: `run_systemd_stop_lock` in the same file has the same shape for the coordinator (`Err(StopCoordinatorError::Io(e)) => print_cli_error(&e.to_string())`, main.rs:1355), which likewise swallows the declared `stop coordinator I/O error:` prefix -- a complete fix should cover both, whereas the neighboring `Err(e)` arm at main.rs:1350 already prints the outer value. (2) The finding does not mention that `plans/impl/2026-06-16-ack-exit-code-convention.md` recorded this inner-only printing as a known caveat and deliberately chose a substring assertion around it; that plan documents the status quo, not a design decision, and the existing test keeps passing after the change.

**Verifier's reasoning:** I read `cli/src/main.rs#handle_pool_lock_error` at line 1198 and it matches the "before" verbatim: `PoolLockError::Io(e) => print_cli_error(&e.to_string())`, while the sibling arm prints `{error}`. `cli/src/pool_lock.rs` declares `#[error("pool lock I/O error: {0}")] Io(#[from] io::Error)` and builds the inner error via `io_from_errno` -> `io::Error::from_raw_os_error`, which carries no path, so the poisoned-lock case exercised by `tests/cli/braid-monitor.py` ("braid ack exits 2 on pool-lock I/O failure", `mkdir /run/braid-pool.lock` -> EISDIR) prints a bare `error: Is a directory (os error 21)`; `rg` shows the wrapper string exists only in the declaration, so it is unreachable in output today. Nothing pins the raw errno text: the VM test asserts only `"directory" in output.lower()` (still true after the fix), and docs/ADR 014, 018, ack.md, monitor.md document only the exit code, never the message. The fix is exactly what the AGENTS "Command error prefixes" convention calls for -- a subsystem-wrapper tag whose inner error would not reveal the failing layer -- and `Io(_)` binds nothing so `error` is not moved and the code still compiles.

**Implemented:** Rendered the outer pool-lock and stop-coordinator errors on all three I/O paths so their braid-layer prefixes reach operators. VM coverage now pins the pool-lock prefix plus both the plain-lock and systemd-stop coordinator prefixes without changing contention behavior or exit codes.

#### TASK-33: `cli/src/recover.rs:34` [low/trivial]

*Lens: error-handling | category: error-taxonomy*

**Claim:** `RecoverError::Probe` is an untagged `#[error("{0}")]` passthrough while every other command-level enum tags the same `ProbeError` wrapper with `probe error:`, so a probe failure during `braid recover` prints with no indication of which braid layer produced it.

**Before:**

~~~~
```rust
pub enum RecoverError {
    #[error("{0}")]
    Probe(#[from] ProbeError),
    #[error("command error: {0}")]
    Cmd(#[from] crate::cmd::CmdError),
```
`probe::probe_pool(...)?` at cli/src/recover.rs:598, :2442 and :2611 converts through this `#[from]`, and `ProbeError::Cmd` renders `command failed: {0}` -- so a failed `btrfs filesystem show` during recovery prints `error: command failed: ...` with no probe attribution.
~~~~

**After:**

~~~~
`#[error("probe error: {0}")] Probe(#[from] ProbeError),` -- matching the identical wrapper in `AddError`, `RemoveError`, `RemoveMissingError`, `ReplaceError`, `StatusError`, and `EnrollKeyFileError`.
~~~~

**Finder's evidence:** AGENTS.md "Command error prefixes": a subsystem-wrapper variant "whose inner error wouldn't reveal which braid layer produced it -- gets a `<subsystem> error:` tag ... Tagging is per-role and codebase-wide." The same doc enumerates the deliberate untagged exceptions (`AddError::ManagedFormatFlag`, `RecoverError::Mount`, `RemoveMissingError::NoMemberForDevid`) and `RecoverError::Probe` is not among them. `rg -n '#\[error("probe' cli/src` shows tagged `Probe` variants in add.rs:130, remove.rs:41, remove_missing.rs:34, replace.rs:144, status.rs:354, enroll_key_file.rs:37 -- recover.rs is the only holdout. No test or doc pins the untagged rendering (`rg -n 'RecoverError::Probe' cli/src` returns only two construction sites).

**Verifier's correction:** Two details are overstated. (1) recover.rs is the only untagged holdout among *command-level* enums that wrap `ProbeError` directly; `cli/src/mount.rs#MountError::Probe` is also `#[error("{0}")]`, but `MountError` is a subsystem enum whose transparent-passthrough role is the documented exception (`RecoverError::Mount`, `UnlockError::Mount`). (2) Consequently the fix only partially achieves the stated goal: probe failures that reach recover through `mount::unlock_and_mount` -> `MountError::Probe` -> `RecoverError::Mount` would still print untagged. This is a documented-convention consistency drift, not a functional bug -- no output is wrong, only unattributed.

**Verifier's reasoning:** The "before" is verbatim accurate: `cli/src/recover.rs:34-35` is `#[error("{0}")] Probe(#[from] ProbeError)`, and `probe::probe_pool(...)?` at :598, :1379, :2442, :2611 plus explicit `RecoverError::Probe(ProbeError::MountInfo(..))` at :616 and :1406 all route through it, so `ProbeError::Cmd`'s `command failed: {0}` reaches the operator as bare `error: command failed: ...`. Every other command-level enum with the identical wrapper tags it -- `add.rs:130`, `remove.rs:42`, `remove_missing.rs:35`, `replace.rs:145`, `status.rs:355`, `enroll_key_file.rs:38`, `ack.rs:413` -- and AGENTS.md names `probe error:` as a canonical subsystem tag, so the same `btrfs filesystem show` failure renders differently under `braid status` than under `braid recover`. I checked the convention's origin plan (`plans/impl/2026-06-26-0917-command-error-prefix-convention.md`): it forbids *removing* tags and reformatting the 45 tagged variants, and its untagged-exception taxonomy covers `RecoverError::Mount` and `Failed(String)` but not `Probe`, which fits neither "terminal hand-authored wording" nor a self-locating passthrough; git history shows the variant untagged since the file's creation commit (cc772804), never revisited. Nothing pins the current rendering: `rg` over `cli/src`, `tests/`, `docs/`, and `README.md` found no assertion on recover's probe message (the one nearby `msg.contains("post-mount probe")` test at recover.rs:17761 exercises `RecoverError::Failed`), and insta snapshots exist only in `ups.rs` and `tui/test_support.rs`.

### Low value, small effort

#### TASK-34: `scripts/docs/check-output-ascii.py:189` [low/small]

*Lens: control-flow | category: correctness*

**Claim:** The buffered clap-doc flush consults `self.line_allow` of the `#[derive(...)]` line rather than of the doc line the hit came from, so the `// ascii-guard: allow` escape hatch is both ignored on `///` clap-help lines and over-broad on the derive line.

**Before:**

~~~~
```python
                elif DERIVE_RE.match(stripped) and CLAP_DERIVE_RE.search(stripped):
                    # Doc block immediately preceding a clap item is its about-text.
                    for ln, ch in self.doc_buffer:
                        if not self.line_allow:
                            self._emit(ln, ch)
```
and the two near-identical loops it pairs with:
```python
    def _scan_doc(self, line: str, text_start: int) -> None:
        if self.skip is not None:
            return
        if self._clap_active():
            for k in range(text_start, len(line)):
                if line[k] in DENY and not self.line_allow:
                    self._emit(self.line_no, line[k])
        else:
            for k in range(text_start, len(line)):
                if line[k] in DENY:
                    self.doc_buffer.append((self.line_no, line[k]))
```
~~~~

**After:**

~~~~
Filter once, at buffering time, so the marker is always evaluated against the line that carries the character; then both loop-invariant `not self.line_allow` guards disappear:
```python
    def _scan_doc(self, line: str, text_start: int) -> None:
        if self.skip is not None or self.line_allow:
            return
        hits = [(self.line_no, ch) for ch in line[text_start:] if ch in DENY]
        if self._clap_active():
            for ln, ch in hits:
                self._emit(ln, ch)
        else:
            self.doc_buffer.extend(hits)
```
and in `_region_line_start`:
```python
                    for ln, ch in self.doc_buffer:
                        self._emit(ln, ch)
```
~~~~

**Finder's evidence:** `self.line_allow` is reset per line in `run()` (line 220), and the `doc_buffer` entries are `(line_no, ch)` tuples that drop the per-line allow state (`_scan_doc`, line 213). By the time the flush runs in `_region_line_start` (line 188), `self.line_allow` reflects the `#[derive(Parser)]` line. Concretely: `/// Do the {EM} thing  // ascii-guard: allow` above a `#[derive(Subcommand)]` enum is still flagged, while `#[derive(Parser)] // ascii-guard: allow` suppresses every buffered doc hit above it. The module docstring says an escape-hatch use "(none exist today)", and `rg 'ascii-guard: allow'` confirms no live usage outside the script itself and a historical plan file, so the fix cannot regress an existing suppression. The `_selftest` "escape marker suppresses" case (line 562) exercises the `eprintln!` string path, not the doc path, so it stays green. Both existing `not self.line_allow` tests are loop-invariant, so hoisting them is behavior-preserving on its own.

**Verifier's correction:** Slightly overstated in one detail: the hatch is only broken on the *buffered* item-level doc path (a `///` block preceding a `#[derive(Parser|Subcommand|Args)]` line). Doc lines inside an already-open clap region (`_clap_active()`) do honor the marker on the correct line today, so the fix makes the buffered path consistent with the path that is already right. Practical caveat worth flagging in the fix: a trailing `// ascii-guard: allow` on a `///` line becomes part of the rendered `--help` text, so the hatch is awkward on doc lines either way -- but that is already true of the clap-active path, so consistency plus the docstring wins. Ideally the change adds selftest cases for both directions (marker on the doc line suppresses; marker on the derive line does not), since the existing "escape marker suppresses" case only exercises the `eprintln!` string path.

**Verifier's reasoning:** The "before" excerpts are verbatim current source (`_region_line_start` flush at scripts/docs/check-output-ascii.py lines 186-191, `_scan_doc` at 203-213), and `self.line_allow` is indeed reset per line in `run()` (line 220) while `doc_buffer` holds only `(line_no, ch)`. I executed the real script against synthetic inputs: `/// Do the {EM} thing // ascii-guard: allow` above `#[derive(Subcommand)]` is still flagged, and `#[derive(Subcommand)] // ascii-guard: allow` silently suppresses the buffered doc hit -- exactly the two failures claimed, one ignoring the hatch and one over-broadly masking violations. This contradicts the script's own docstring ("suppress the check for that line") and the plan's implementation note ("a line carrying the marker is exempt", plans/impl/2026-06-04-cli-output-ascii-guard.md); nothing in AGENTS.md, principles.md, or the ADRs designs a derive-line block-suppression, and `rg ascii-guard` shows no live marker anywhere in the tree. I applied the proposed "after" to a scratch copy: it fixes both cases, `--selftest` stays green, and a full tree scan with both versions yields 0 failures, so it is behavior-preserving today.

#### TASK-35: `cli/src/recover.rs:1419` [low/small]

*Lens: control-flow | category: control-flow*

**Claim:** `plan_recover` re-tests `open_plan.is_some()` three times and `is_replace_pool_mutation(&journal.op)` twice across three consecutive blocks that all key off the same two facts.

**Before:**

~~~~
```rust
    let mut actions = Vec::new();
    if open_plan.is_some() {
        actions.push(RecoverWorkAction::InitialOpenPool);
    }

    if is_replace_pool_mutation(&journal.op) && open_plan.is_some() {
        actions.push(RecoverWorkAction::WaitForKernelReplace);
    }

    if let Some(initial_open_plan) = &open_plan
        && is_replace_pool_mutation(&journal.op)
    {
        let mut cycle_reopen_names: Vec<DiskName> = Vec::new();
        ...
```
~~~~

**After:**

~~~~
```rust
    let mut actions = Vec::new();
    if let Some(initial_open_plan) = &open_plan {
        actions.push(RecoverWorkAction::InitialOpenPool);
        if is_replace_pool_mutation(&journal.op) {
            actions.push(RecoverWorkAction::WaitForKernelReplace);
            let mut cycle_reopen_names: Vec<DiskName> = Vec::new();
            ...
        }
    }
```
~~~~

**Finder's evidence:** Push ordering is preserved: today block 1 pushes `InitialOpenPool`, block 2 pushes `WaitForKernelReplace`, block 3 pushes `RemountCycle`; the merged form pushes them in the same sequence. The `return Err(PlanFailure::with_notes(...))` early exits inside block 3 (recover.rs:1439-1478) still occur after `WaitForKernelReplace` has been pushed, exactly as today, and the pushed `actions` vector is discarded on those error paths anyway. Neither `open_plan` nor `journal.op` is mutated between line 1418 and line 1484.

**Verifier's correction:** The redundancy is even deeper than claimed: the guard at recover.rs:1352 already returns `Err` when `open_plan.is_none() && is_replace_pool_mutation(&journal.op)`, so by line 1419 a replace-PoolMutation journal *implies* `open_plan.is_some()` — blocks 2 and 3 re-test a fact the function has already refused the negation of. The `if let` in block 3 is still load-bearing as a binding (`initial_open_plan.any_missing_member` at line 1482), so the merge must keep the `if let Some(initial_open_plan) = &open_plan` outer form exactly as the "after" proposes rather than collapsing to a bool. Behavior-only value is zero; the gain is making "replace-specific actions only exist on the just-mounted path" structural instead of restated three times.

**Verifier's reasoning:** The "before" snippet matches `cli/src/recover.rs` lines 1418-1429 verbatim: block 1 tests `open_plan.is_some()`, block 2 tests `is_replace_pool_mutation(&journal.op) && open_plan.is_some()`, block 3 tests `if let Some(..) = &open_plan && is_replace_pool_mutation(..)` — note the operand order even flips between blocks 2 and 3, which argues accretion rather than design. Nothing mutates `open_plan`, `journal`, `report`, or `actions` between the blocks, and every early `return Err(PlanFailure::with_notes(notes, ..))` inside block 3 discards `actions` anyway, so the merged nesting preserves push order (InitialOpenPool, WaitForKernelReplace, RemountCycle) and all error paths; borrowck is unaffected since block 3 already moves `notes` on diverging paths inside an `if let` borrowing `open_plan`. `git show f60c8b3b` (the "unify plan execution around typed work plans" refactor that introduced all three pushes in one commit) shows the pre-refactor code was a *single* combined guard `if just_mounted && is_replace_pool_mutation(&journal.op) { wait_for_kernel_replace_to_finish(..); relock_and_remount(..) }` — so the flat repetition is a mechanical translation artifact, not deliberate. Nothing in AGENTS.md, `docs/design/principles.md`, or `docs/internals/btrfs/dev-replace-resume.md` (which only pins the two-action *split*, which the merge keeps) is touched; the action enum, rendered Steps, and executor are all unchanged.

#### TASK-36: `cli/src/parse/btrfs_device_stats.rs:134` [low/small]

*Lens: doc-drift | category: doc-drift*

**Claim:** A test doc comment cites `cli/docs/command-capabilities.md`, a file deleted from the repo in April 2026.

**Before:**

~~~~
    /// Unknown fields from future btrfs-progs versions are silently ignored.
    /// Known fields still parse correctly. See cli/docs/command-capabilities.md.
    #[test]
    fn device_stats_ignores_unknown_fields_parses_known() {
~~~~

**After:**

~~~~
    // Intent: unknown fields from future btrfs-progs versions are silently
    //   ignored while known fields still parse correctly.
    // Why it exists: the tolerant-parse contract in
    //   docs/dev/parser-compatibility.md is what lets braid survive a
    //   btrfs-progs bump that adds JSON keys.
    // Scenario: a nixpkgs bump ships btrfs-progs with an extra device-stats key.
    #[test]
    fn device_stats_ignores_unknown_fields_parses_known() {
~~~~

**Finder's evidence:** `cli/` contains only Cargo.toml, plans, src, target, tests -- there is no `cli/docs/`. `git log --all -- '*command-capabilities*'` shows commit 126438a7 "delete dead docs" removing `cli/docs/command-capabilities.md`. The reference is invisible to the repo's own linters: `scripts/docs/check-code-doc-anchors.py` only validates `docs/*.md#anchor` citations, so a bare `cli/docs/...` path is never resolved. The rewrite also brings the preamble into the `//` Intent/Why/Scenario form that docs/dev/testing.md#preamble-literal-line-comment-form mandates and that commit 657f03a6 has been converting tests to.

**Verifier's correction:** The stale `cli/docs/command-capabilities.md` citation is real, but it appears twice (btrfs_device_stats.rs line 134 and btrfs_device_usage.rs line 282) and the proposed replacement target is wrong: docs/dev/parser-compatibility.md documents fixture lanes and refresh triggers, not the tolerant-parse contract, so citing it would substitute a false reference for a dead one. Correct fix: convert both preambles to the `//` Intent/Why/Scenario form and either drop the doc pointer entirely (state the rationale inline -- btrfs-progs may add per-device stat/allocation keys on a routine bump) or first restore the fail-hard-default + tolerant-exceptions policy from the deleted `cli/docs/command-capabilities.md` into a live doc and cite that with a real `#anchor` so check-code-doc-anchors.py can enforce it.

**Verifier's reasoning:** Verified the "before" verbatim at cli/src/parse/btrfs_device_stats.rs lines 133-134, and the target is genuinely gone: `ls cli/` shows no `docs/` dir, and `git show --stat 126438a7` ("delete dead docs", 2026-04-08) removed `cli/docs/command-capabilities.md`. The dead citation escapes CI because `scripts/docs/check-code-doc-anchors.py`'s `CITE_PATTERN = re.compile(r"(docs/[A-Za-z0-9_./-]+\.md)#([A-Za-z0-9_-]+)")` requires a `#anchor`, so a bare path is never resolved -- so this is drift, not something the repo deliberately keeps. Two details are off, though: (a) the identical dead reference also sits at `cli/src/parse/btrfs_device_usage.rs#device_usage_ignores_unknown_keys` (line 282), so fixing only one site leaves the drift half-repaired; (b) I read all of docs/dev/parser-compatibility.md and grepped docs/ for "unknown field|silently ignor|fail-hard" -- that doc covers fixture lanes and refresh triggers and says nothing about tolerant parsing, so the proposed "after" swaps a dead citation for a false one. The deleted file (recovered via `git show 126438a7^:cli/docs/command-capabilities.md`) held the actual fail-hard-default + per-parser tolerant-exceptions table, and that policy now exists nowhere in docs/. The preamble reformat to `//` Intent/Why/Scenario is correct per docs/dev/testing.md#preamble-literal-line-comment-form (2599 tests, ~30 files already use `// Intent:`), so the rewrite direction is right.

#### TASK-37: `cli/src/online_state.rs:265` [low/small]

*Lens: error-handling | category: dead-error-type*

**Claim:** `mark_online` and `mark_offline` are typed `Result<(), OnlineError>` but return `Ok(())` on every path (all failures are warned in place), so both call sites need a `let _ =` that reads like a swallowed error when nothing is actually being discarded.

**Before:**

~~~~
```rust
pub fn mark_online(
    snap: Option<&OnlineSnapshot>,
    cfg: &Config,
    ops: &dyn OnlineStateOps,
) -> Result<(), OnlineError> {
```
and line 341: `pub fn mark_offline(cfg: &Config, ops: &dyn OnlineStateOps) -> Result<(), OnlineError> {`. Callers: `cli/src/online_state.rs:331` `let _ = mark_online(snap, cfg, ops);` and `cli/src/lock.rs:1185` `let _ = mark_offline(config, online_ops);`.
~~~~

**After:**

~~~~
Change both signatures to return `()` and drop the trailing `Ok(())`; the two call sites become plain `mark_online(snap, cfg, ops);` / `mark_offline(config, online_ops);` with no `let _ =`, making it obvious at a glance that these finalizers are warn-and-continue by design. The three `mark_offline(&cfg, &ops).unwrap()` calls in the module's own tests lose their `.unwrap()`.
~~~~

**Finder's evidence:** Every exit in `mark_online` is `Ok(())`: the mountpoint-probe failure returns `Ok(())` at line 271, the `!mounted` early return at 275, chown/chmod failures only `eprintln!` (279-290), the systemctl-start failure only `eprintln!` (298-302), and the function ends with `Ok(())` at 317. `mark_offline` mirrors this: `Ok(true) => return Ok(())` at 344, mountpoint-error `return Ok(())` at 351, systemctl-stop failure only `eprintln!` at 358, then `Ok(())` at 360. Neither has a `?` or an `Err(...)` construction. `LockOrchestrateError` (cli/src/lock.rs:55) has no mark-offline variant, confirming the caller never intended to propagate one.

**Verifier's correction:** The core claim holds, but the "after" undercounts the test churn: besides the three `mark_offline(&cfg, &ops).unwrap()` calls, six `mark_online(...).unwrap()` calls in the same test module (in `mark_online_skips_systemctl_when_lifecycle_disabled`, `mark_online_applies_pool_access_group_without_lifecycle`, `mark_online_starts_when_lifecycle_enabled` (x3), and `mark_online_skips_systemctl_when_snapshot_absent`) also lose their `.unwrap()`. Also note the category label "dead-error-type" is imprecise -- `OnlineError` is not dead (it is the `OnlineStateOps` trait's error type and is matched on in `cli/src/lock.rs#stop_unit_warn_on_error`); only the two functions' `Result` wrappers are vestigial.

**Verifier's reasoning:** I read `cli/src/online_state.rs` in full: the "before" snippets match byte-for-byte (`mark_online` signature ends at line 265, `mark_offline` at 341), and every exit in both functions is `Ok(())` -- no `?`, no `Err(...)` construction anywhere in either body, with all failures handled by `eprintln!` warnings. `rg` shows exactly two non-test callers, both `let _ =` (`online_state.rs:331` inside `run_with_online_marker`, `lock.rs:1185` inside `cmd_lock_orchestrate_impl`), and `LockOrchestrateError` has only `CmdLock`/`MarkDone` variants, so no propagation was ever wired up. The design intent supports collapsing rather than contradicting it: ADR 020 states `mark_online` "warns and exits successfully", ADR 018/026 document the warn-and-skip fail-safes, and the originating plan `plans/impl/2026-05-19-rust-owned-pool-operation-lock.md` says outright "Each step's failure is logged as WARNING to stderr but does not return Err" while specifying sibling `snapshot()` as infallible with a plain return type -- so the module already has precedent for the proposed shape. `OnlineError` itself stays alive (the `OnlineStateOps` trait and `lock.rs#stop_unit_warn_on_error` both use it), so nothing else breaks; no ASCII-output, error-prefix, or doc-comment convention is touched.

## Rejected on verification

### TASK-38: `cli/src/lock.rs:462` (control-flow)

**Claim:** `umount_with_retry` has two adjacent branches that build and return the identical `build_umount_error(...)` value, duplicating the terminal-failure path.

**Why rejected:** The "before" snippet matches `cli/src/lock.rs#umount_with_retry` verbatim (lines 461-467), and the merge is indeed behavior-preserving since `build_umount_error` is a pure formatter that re-derives the busy hint from `stderr`. But this is a deliberate house shape, not accidental duplication: `cli/src/mapper_close.rs#close_mapper_with_retry` has the byte-identical skeleton (`if !is_busy { return Err(...Failed) } if attempt == N { return Err(...DeviceBusy) }`), where the two branches return *different* error variants and provably cannot be collapsed into a `||`. The promoted plan `/Users/dan/Code/braid/plans/impl/2026-05-25-lock-umount-busy.md` prescribes this exact code and states the helper "mirrors `close_mapper_with_retry` in shape", and `build_umount_error`'s doc comment exists specifically to keep "retry exhaustion and non-busy failures" -- two named, separate paths -- on one operator-facing contract; the proposed merge collapses those two roles, leaves the helper with a single call site, and makes hint-vs-no-hint depend implicitly on which disjunct fired. That is a readability tradeoff against a documented rationale, i.e. taste plus something the repo deliberately does, so it fails criteria 2 and 4.

### TASK-39: `tests/eval/grammar-parity.nix:1` (test-gaps)

**Claim:** The grammar-parity check's stated intent is Nix/Rust parity, but it only evaluates the Nix predicates against a Nix-local sample list -- the Rust side is never invoked, so a one-sided grammar change passes.

**Why rejected:** The quoted preamble and body of tests/eval/grammar-parity.nix are accurate, and I diffed the three accept/reject lists against cli/src/types.rs (mount_point_* at 1332/1351, ups_name_* at 1373/1386, interface_* at 1404/1417): they are byte-identical today, so no drift exists. The decisive point is that the finding's stated failure mode is false -- a one-sided grammar edit does NOT pass. Weakening modules/braid/grammar.nix (e.g. dropping the <=15 length check in isValidInterface) makes eval-grammar-parity fail on "abcdefghijklmnop" under `nix flake check`; weakening the Rust newtype fails the corresponding cargo test, which runs in CI (release.yml `just test-rust`, plus crane's default doCheck during the package build). Both lanes execute the same matrix in their own language, which is exactly what the impl plan (plans/impl/2026-06-22-validate-argv-input-boundaries.md, "asserting the exact lists the Rust `parse` tests use ... the predicate check owns matrix parity") deliberately specified, and the Intent line never claims Rust is invoked. The only residual gap is a sample added to one list but not the other -- that silently narrows coverage, it never lets a divergent grammar pass -- and the proposed shared JSON does not make the Intent line any more accurate, since divergence at inputs outside the matrix remains unchecked either way. The fix also has an unstated cost: cli's crane `src` filter (flake.nix, which already special-cases tests/fixtures and docs/commands/status.md) excludes tests/eval/, so an `include_str!("../../tests/eval/grammar-samples.json")` would break the Nix build until the filter is extended.
