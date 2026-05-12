You are the main orchestrator for implementing the Braid LUKS UUID identity migration.

Treat this as one large coherent migration. Do not try to force every worker phase into an independently compiling commit. Intermediate compile failures are expected until the identity rekey has propagated through the command surface.

Prompt authority:

- This prompt is the implementation handoff. Do not edit the selected migration plan unless the user explicitly asks you to revise the plan.
- The selected migration plan is the technical source of truth. Worker phase summaries in this prompt are routing aids only.
- If the selected plan says agents should refine only because it lives in `plans/todo/`, this prompt overrides that workflow gate only: implement from the selected plan, but keep all of the plan's technical requirements, Open Decision gates, Discovered TODO gates, test obligations, and Definition of Done.
- If this prompt and the selected plan conflict on implementation behavior, the plan wins. If the conflict is about whether implementation is authorized, this prompt wins. If the conflict cannot be separated that way, stop and report the ambiguity before editing.

Plan resolution:

- First, inspect the repo and locate the migration plan.
- Prefer an exact user-provided plan path if one was provided in the surrounding Claude Code session.
- Otherwise, use `plans/impl/*luks*uuid*identity*migration*.md` if there is exactly one match.
- Otherwise, use `plans/todo/plan-luks-uuid-identity-migration.md`.
- If more than one plausible plan exists, stop and ask which one to use.
- Record the selected plan path in the task list and in the final response.
- If the selected plan lives in `plans/todo/`, note in the preflight task that implementation is authorized by this prompt and that the plan file itself must not be edited.

Initial setup:

- Read `AGENTS.md`.
- Read the full migration plan yourself as the orchestrator.
- Check the plan's Open Decisions and Discovered TODO Log before implementation.
- If any blocking Open Decision or blocking TODO is still open, stop and report it before spawning workers.
- Inspect `git branch --show-current`, `git status --short --untracked-files=all`, `git diff`, `git diff --cached`, and `git log --oneline -10`.
- Work on the current branch. Do not require the branch to be `master`, do not switch branches, and do not rebase or merge.
- Classify dirty work before spawning workers:
  - unrelated staged work: stop and ask for it to be cleared;
  - unrelated tracked unstaged work: stop if it touches files this migration is likely to edit, otherwise list it in the preflight task and ignore it;
  - unrelated untracked scratch files: list them in the preflight task, ignore them, and never stage them;
  - untracked files at paths the migration is expected to create or edit: stop and ask whether they are in scope.
- Do not revert user changes or prior-agent changes.
- Do not use `git stash`.
- Do not push.
- Before spawning Phase 1, update the Phase 0 task with: selected plan path, current branch, dirty-work classification, Open Decision/TODO status, and whether implementation is authorized from a `plans/todo/` path.

Task tracking:

- Use Claude Code's built-in task list throughout this run.
- In interactive Claude Code, use `TaskCreate`, `TaskUpdate`, `TaskList`, and `TaskGet` as appropriate.
- The main orchestrator owns the global task list. Worker subagents may report progress, but their internal task state is not authoritative.
- Create and maintain top-level tasks for:
  - Phase 0: preflight and plan/worktree audit
  - Phase 1: core identity/data model
  - Phase 2: journal schema and cryptsetup `luksFormat` boundary
  - Phase 3: mutating command migration
  - Phase 4: recovery, discovery, and lock migration
  - Phase 5: remaining command surfaces, docs, scripts, fixtures, and tests
  - Phase 6: whole-repo audit, final verification, and implementation review
- Keep exactly one implementation phase marked `in_progress` at a time.
- After each worker returns, update the task with:
  - what changed,
  - what checks were run,
  - whether compile/test failures are expected or need fixing,
  - the next handoff.
- Mark a phase `completed` only after you have inspected the diff and accepted the worker output.
- Keep Phase 0 `in_progress` until preflight is complete. Do not spawn implementation workers during Phase 0.

Subagent strategy:

- Use worker subagents sequentially, not in parallel.
- The orchestrator owns the overall migration, reads the full plan, maintains the global task list, verifies worker output, and decides when phases are accepted.
- The orchestrator builds every worker prompt dynamically from the selected plan, current source tree, current diff, current task list, and current compiler/test state. Do not reuse stale phase prompts.
- Worker phase summaries in this prompt are routing aids only. The selected migration plan is the implementation spec.
- Each worker must be told that it is not alone in the codebase, must not revert user/prior-agent edits, and must adapt to the current tree.
- Workers have broad read freedom and phase-bounded write authority.
- Each worker should start from the assigned plan sections and relevant source files, then inspect any other plan section, source file, test, fixture, doc, script, or local reference source needed to make correct decisions.
- Each worker may edit any file necessary to complete its assigned phase. Expected scope is a guide, not a hard file sandbox.
- Edits outside the expected phase scope must be directly necessary for the phase, kept minimal, and explained in the worker final response.
- After each worker returns, inspect the diff yourself before continuing.
- If a worker causes failures that should be fixed within that phase, send the failure output back to the same worker and require a fix.
- If the plan is wrong or underspecified, stop and surface the plan gap rather than silently inventing behavior.

Dynamic subagent prompt protocol:

- Before spawning each worker, locate the current plan headings with `rg -n '^#{1,3} ' <plan>`.
- Build the worker prompt from the current plan text and current repo state, not from memory.
- For the worker's expected files, symbols, commands, tests, docs, and fixtures, search the plan for each relevant filename, type, function, command, and test name.
- The worker prompt must include:
  - the phase goal,
  - current handoff state from prior phases,
  - exact plan section names the worker should start with,
  - expected source/test/doc/fixture areas,
  - the broad-read and phase-bounded-write policy,
  - the global invariants below,
  - phase-specific invariants copied or summarized from the plan,
  - exact error/remediation wording from the plan when relevant,
  - exact tests/checks the plan calls for in that scope,
  - relevant Open Decisions or Discovered TODO entries,
  - relevant Risk Register entries,
  - expected compile state after the phase.
- Do not give a worker only the phase summary from this prompt. The phase summary is a routing aid; the plan is the source of truth.
- Use this worker prompt shape as a starting template and fill it with phase-specific details:

```text
You are implementing one bounded phase of the Braid LUKS UUID identity migration.

You are not alone in the codebase. Do not revert user or prior-agent changes. Adapt to the current working tree.

Selected plan: <path>
Current phase: <phase name>
Current handoff state:
- <summary of completed prior phases, current compile/test state, and known expected failures>

Start by reading:
- AGENTS.md
- <exact plan sections to start with>
- <relevant source/test/doc/fixture files>

You may inspect any other plan section, source file, test, fixture, doc, script, or local reference source needed to make correct decisions. If another section constrains your work, read it and mention it in your final response.

Expected phase scope:
- <expected source/test/doc/fixture areas>

Write authority:
- You may edit any file necessary to complete this phase.
- Expected scope is a guide, not a hard sandbox.
- Any edit outside expected scope must be directly necessary, minimal, and explained in your final response.

Global invariants:
- LUKS UUID is the persistent disk identity.
- Disk name is presentation/adoption metadata, not identity.
- by-id path is hardware addressing, not identity.
- btrfs devid is live filesystem state and may be used only where the plan permits.
- Mapper names and LUKS labels must not be used as persistent identity except where the plan explicitly allows display/adoption behavior.
- There is no backwards compatibility requirement.
- User-facing CLI output must use plain ASCII and `--`, not em dashes.
- New Rust `pub` or `pub(crate)` items need concise doc comments when required by AGENTS.md.
- Parser behavior must be grounded in local `reference/` sources before making assumptions.

Before editing, write a short phase obligation list from the plan and source inspection:
- data shape changes
- command behavior changes
- error/remediation wording
- tests to add/update
- docs/fixtures/scripts if relevant
- expected compile limitations
- plan sections and source areas inspected

Then implement against that obligation list.

If the plan and this prompt conflict, the plan wins for technical behavior. If the plan itself is ambiguous or contradicted by source reality, stop and report the ambiguity rather than inventing behavior.

Worker final response must include:
- plan sections read
- source/test/doc/reference files inspected
- files changed
- obligations completed
- obligations deferred and why
- edits outside expected scope and why they were necessary
- checks run and results
- expected vs unexpected compile/test failures
- proposed new orchestrator tasks, if any, using this format:
  - Title:
  - Type: blocking | later-phase | verification | docs | cleanup | risk
  - Evidence: <plan section, source file, compiler/test output, or observed behavior>
  - Recommended owner phase:
  - Blocks continuing: yes/no
```

Worker task discovery protocol:

- Workers may propose new orchestrator tasks when they discover real work that should not be completed in the current phase.
- Proposed tasks must include title, type, evidence, recommended owner phase, and whether they block continuing.
- The orchestrator must verify proposed tasks before adding them to the global Claude Code task list.
- Do not add vague, duplicate, speculative, or unevidenced tasks.
- Blocking proposed tasks stop phase advancement until resolved or explicitly accepted as a plan gap.

Worker acceptance protocol:

- After each worker returns, inspect `git status --short --untracked-files=all`, `git diff --stat`, and the full diff for every changed file.
- Compare the worker's diff and final response against the selected plan, the worker's obligation list, relevant Risk Register entries, and current compiler/test output.
- Verify every edit outside expected phase scope is necessary, minimal, and explained. Send unexplained or drifting edits back to the same worker for correction.
- Verify any proposed new tasks before adding them to the global task list.
- Search for phase-specific stale assumptions before acceptance. Examples: name-keyed membership after Phase 1, stale journal value-side UUID fields after Phase 2, name-derived mutation targets after Phase 3, mapper-name identity in lock/recover/discover after Phase 4, stale docs/fixtures after Phase 5.
- Run the cheap checks the plan names for that phase when they can run meaningfully. Once the tree is expected to compile, run `cargo check --workspace` after every worker phase before continuing.
- If a failure is unexpected for that phase, send the failure output back to the same worker and require a fix before moving on.
- If a failure is expected because later phases have not migrated their call sites yet, record the exact expected failure class in the task note and continue.
- Mark a phase completed only after accepting the diff, recording known failures, and adding verified follow-up tasks.

Global invariants:

- LUKS UUID is the persistent disk identity.
- Disk name is presentation/adoption metadata, not identity.
- by-id path is hardware addressing, not identity.
- btrfs devid is live filesystem state and may be used only where the plan permits.
- mapper names and LUKS labels must not be used as persistent identity except where the plan explicitly allows display/adoption behavior.
- There is no backwards compatibility requirement. Braid is unreleased; old formats should be changed everywhere, not supported with shims.
- User-facing CLI output must use plain ASCII and `--`, not em dashes.
- New Rust `pub` or `pub(crate)` items need concise doc comments when required by `AGENTS.md`.
- Parser behavior must be grounded in local `reference/` sources before making assumptions.

Commit strategy:

- Worker subagents must not create commits.
- After accepting each worker phase, the orchestrator should create a checkpoint commit for that phase.
- Checkpoint commits may be compile-broken only when the selected plan or accepted phase notes say the breakage is expected until a later phase.
- Broken checkpoint commit subjects must start with `wip:`.
- Each checkpoint commit body must record the accepted phase, known expected failures, and checks run.
- The task list remains the authoritative phase tracking mechanism; checkpoint commits are for git-level traceability and rollback.
- Before final delivery, the orchestrator should decide whether to keep checkpoint history or squash/rework it into clean implementation commits.
- Final history should be clean if it will be merged directly: preferably one implementation commit, with docs/fixtures split only if that separation is genuinely useful.
- If committing, stage touched paths by name. Never use `git add -A`, `git add .`, or directory-wide staging.
- Commit messages must be Conventional Commits style and the first line must start lowercase.

Verification strategy:

- During intentionally broken phases, run cheap targeted checks where useful, but do not spend time forcing the whole repo green before the rekey has propagated.
- Once command surfaces are migrated far enough for compilation, run `cargo check` or the repo-preferred Rust check continuously while finishing later phases.
- Before final delivery, run the strongest practical verification matrix from the plan and `AGENTS.md`.
- Prefer `just test-rust` over remembering crate package names.
- Run VM tests only when the implementation reaches a coherent state and the plan/test matrix calls for them.
- If parser behavior changed, run the parser tests and fixture workflow required by `AGENTS.md`.

Worker phase briefs:

Phase 1: core identity/data model

- Assigned plan sections:
  - Goal
  - Identity Rules
  - New Data Model / Value Types
  - New Data Model / Membership Shape
  - New Data Model / `LuksUuidMap`
  - New Data Model / Membership API
  - Execution Checklist
  - relevant Rust Unit Tests
  - relevant Risk Register entries
- Scope:
  - `LuksUuid`, `DiskName`, `ByIdPath`, `LuksFormatExtraOpts`
  - `LuksUuidMap`
  - `PoolMembership` and `DiskMember` shape
  - membership serialization/deserialization invariants
  - test helpers such as `test_uuid(seed)` and `disk_member(seed, name, by_id)` if the plan calls for them
- Expected result:
  - core types and membership model are rekeyed,
  - focused unit tests are added where possible,
  - compile may still be broken because call sites have not been migrated.

Phase 2: journal schema and cryptsetup `luksFormat` boundary

- Assigned plan sections:
  - LUKS Format Boundary
  - LUKS Format Boundary / `CmdRequest::CryptsetupLuksUuid` request shape (pinned)
  - LUKS Format Boundary / Recording/dry-run runner trait surface (pinned)
  - Journal Schema
  - relevant Rust Unit Tests
  - relevant Risk Register entries
- Scope:
  - explicit `uuid` and `label` for `cryptsetup luksFormat`
  - validated extra opts for `cryptsetup luksFormat`, including rejection of plan-specified managed flags
  - command rendering tests
  - journal structs and pending-op JSON shape
  - pending-op parse/remediation behavior
- Expected result:
  - boundary and journal structures match the plan,
  - focused tests exist for rendering and JSON shape,
  - compile may still be broken until command call sites migrate.

Phase 3: mutating command migration

- Assigned plan sections:
  - Command Migration / Shared Patterns
  - `membership.rs`
  - `add.rs`
  - `remove.rs`
  - `remove_missing.rs`
  - `replace.rs`
  - relevant Test Plan sections
  - relevant Risk Register entries
- Scope:
  - resolve user-facing names to LUKS UUIDs at command boundaries,
  - use UUID identity through add/remove/remove-missing/replace,
  - keep mapper names and labels out of persistent identity,
  - add/update behavioral tests.
- Expected result:
  - mutating command surfaces are migrated,
  - the compiler checklist shrinks substantially,
  - compile may still depend on recovery/discovery/lock and remaining command surfaces.

Phase 4: recovery, discovery, and lock migration

- Assigned plan sections:
  - `recover.rs`
  - `discover.rs`
  - `lock.rs`
  - Single-User Cutover
  - `discover --write --expect-count`
  - parser/discovery-related Rust Unit Tests and VM Tests
  - relevant Risk Register entries
- Scope:
  - recovery identifies targets by LUKS UUID,
  - discovery reads UUID from `cryptsetup luksDump`,
  - lock close-set design uses UUID ownership,
  - parser changes are grounded in local `reference/cryptsetup` sources,
  - tests prove UUID ownership and recovery behavior.
- Expected result:
  - recovery/discovery/lock are migrated,
  - parser behavior is tested,
  - whole-repo compile should be close or available depending on remaining surfaces.

Phase 5: remaining surfaces, docs, scripts, fixtures, and tests

- Assigned plan sections:
  - Other Rust Touch Points
  - Documentation
  - Single-User Cutover
  - Test Plan
  - Definition of Done
  - Risk Register
- Scope:
  - mount, unlock, status, TUI, doctor, enroll, preflight, main, browse, and any other stale call sites,
  - README and design docs required by `AGENTS.md`,
  - scripts,
  - VM tests,
  - fixtures and snapshots,
  - stale docs or stale assumptions.
- Expected result:
  - `cargo check` should pass before broad fixture/snapshot/doc cleanup is considered complete,
  - relevant Rust and VM tests are added/updated,
  - docs match behavior.

Phase 6: whole-repo audit and final review

- Do this yourself as orchestrator, using subagents only for bounded fixes.
- Compare the final implementation against the selected plan and the current PR branch's intended base. Do not assume the branch is `master`; if the base is unknown, state what comparison you used.
- Search for stale assumptions:
  - disk name used as persistent identity,
  - mapper name used as identity,
  - LUKS label used as identity,
  - value-side duplicate `luks_uuid`,
  - value-side duplicate `mapper_name`,
  - stale docs/scripts/fixtures.
- Search both code and docs. Use targeted `rg` searches plus a diff review; do not rely only on tests.
- Re-read the plan's Definition of Done and verify each item.
- Re-read Open Decisions and Discovered TODO Log and verify nothing blocking remains.
- Run the strongest practical test matrix.
- Perform a review-impl-style audit against the final diff and the plan.
- Fix findings before final report.

Final response:

- State whether the implementation matches the plan.
- List commits created, if any.
- List checks/tests run and results.
- List skipped checks and why.
- List remaining risks or blockers.
- Mention any unrelated dirty/untracked files that were intentionally ignored.
- Do not claim success if the tree does not compile or required tests were skipped without explanation.
