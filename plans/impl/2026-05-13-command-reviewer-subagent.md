# Refactor command review orchestration: extract briefing into a custom subagent

## Context

The existing command review prompt content orchestrates a parallel
fan-out: one investigative subagent per public `braid` CLI command,
each writing a findings file to `./command-findings/<slug>.md`. The
current prompt inlines a ~70-line briefing that is templated and
resent on every `Agent` call.

Reviewed against the live Claude Code docs, several friction points
are docs-confirmed:

- Subagents start in a fresh context window with only the prompt
  string -- the parent conversation is not passed
  ([sub-agents.md][1]).
- CLAUDE.md auto-loads in the subagent. braid's `CLAUDE.md` does
  `@AGENTS.md`, so the briefing's "Read `AGENTS.md`" step is
  redundant.
- Project-scoped subagents are defined as `.claude/agents/<name>.md`
  with frontmatter for `description`, `tools`, `model`, `maxTurns`,
  `permissionMode`, etc. ([sub-agents.md][1]).
- Subagent working-directory changes don't bleed back to the parent
  ([tools-reference.md][2]); permissions are prompted just like the
  main conversation.
- Frontmatter `tools:` declares tool _availability_, not permission
  grant -- a subagent with `Write` listed still needs a matching
  `permissions.allow` entry in `.claude/settings*.json` to write
  without prompting. Background subagents auto-deny tool calls that
  would otherwise prompt; foreground subagents prompt the user for
  each call ([tools-reference.md][2]).
- Path-scoped file-modification rules use `Edit(...)`, which covers
  all file-modification tools (`Write`, `Edit`, `NotebookEdit`).
  There is no separate `Write(<path>)` rule shape. A single leading
  slash in the path pattern is _project-relative_; a double leading
  slash is filesystem-absolute ([permissions.md][4]).
- `TaskCreate` accepts a single task per call (not a batch); each
  call returns a `taskId` used by subsequent `TaskUpdate` calls
  ([tools-reference.md][2]). The previous `TodoWrite` bulk pattern is
  deprecated.
- Custom subagent frontmatter requires both `name` and `description`;
  the agent name is **not** derived from the filename
  ([sub-agents.md][1]).

What the docs do **not** specify (and which I previously stated as
fact):

- Any parallel-fan-out concurrency cap or batch-size recommendation.
- Whether subagent return values should be kept short. Still
  worthwhile as a practice (parent context budget), but framed as
  advice not as a rule.
- `run_in_background` mechanics for subagents.

Confirmed counts (from `cli/src/main.rs`):

- 16 leaf top-level commands (each invocable as a complete `braid
<cmd>`): `add`, `remove`, `remove-missing`, `replace`, `status`,
  `doctor`, `unlock`, `lock`, `enroll`, `idle`, `monitor`, `ack`,
  `tui`, `browse`, `discover`, `recover`. Note effective Clap name
  `enroll` (declared via `#[command(name = "enroll")]` on the
  `EnrollKeyFile` variant at `cli/src/main.rs:42`); the slug must
  use that, not the variant name.
- `ups` is parent-only (no action of its own; `UpsArgs` only carries
  a subcommand) -- it is **not** a review target on its own. The
  only reviewable invocation under it is `ups status`.
- Total leaf review targets: **17** (16 top-level + `ups status`).
- `manual/commands/` has 17 pages (16 top-level + `ups-status.md`)
  -- matches the leaf target set 1:1.
- 3 hidden commands excluded (`scrub-cancel`, `scrub-needs-resume`,
  `scrub-resume-or-start`, all `#[command(hide = true)]`).
- No existing `.claude/agents/` directory; safe to create.
- `command-findings/` does not exist; safe to create.
- No `Edit(/command-findings/**)` permission rule in
  `.claude/settings.local.json` today; this plan adds it as a
  committed change (see Files section). The pre-flight then
  verifies presence only -- no runtime mutation.

## Goal

Two outcomes:

1. Move the long per-command briefing into a project-versioned
   custom subagent at `.claude/agents/command-reviewer.md`.
2. Add `prompts/command-review-fanout.md` as a short orchestrator
   that does discovery, pre-flight, task tracking, and a
   single-message parallel fan-out to `command-reviewer`.

No source code changes; no behavior change to the CLI.

## Files

**Create:**

- `/Users/dan/Code/braid/.claude/agents/command-reviewer.md` --
  project-scoped custom subagent. Frontmatter + system prompt
  containing the current briefing (sections: "Your job", "Become
  an expert", "Project conventions", "Write findings to", findings
  template).
- `/Users/dan/Code/braid/prompts/command-review-fanout.md` --
  short orchestrator prompt with discovery, pre-flight, task
  tracking, fan-out, and rollup instructions.

**Modify:**

- `/Users/dan/Code/braid/.claude/settings.local.json` -- add
  `Edit(/command-findings/**)` to `permissions.allow`. This is a
  tracked file (`git ls-files .claude/settings.local.json`
  matches), so the rule is committed as part of this change, not
  added at runtime. Single leading slash is project-relative per
  [permissions.md][4]. `Edit(...)` scopes all file-modification
  tools (`Write`, `Edit`, `NotebookEdit`) -- there is no separate
  `Write(...)` rule shape.

**Created at runtime (not by this plan):**

- `/Users/dan/Code/braid/command-findings/` and one file per
  command (`add.md`, ..., `ups-status.md`). The orchestrator
  pre-creates the directory.

## `.claude/agents/command-reviewer.md` shape

Frontmatter (per [sub-agents.md][1]):

```yaml
---
name: command-reviewer
description: >
  Reviews one public braid CLI command end-to-end for correctness,
  testing coverage, simplicity opportunities, and project fit.
  Writes findings to ./command-findings/<slug>.md and returns
  only a one-line summary.
tools: Read, Grep, Glob, Bash, WebFetch, WebSearch, Write
model: opus
effort: xhigh
---
```

Notes:

- `name` is required by Claude Code (it does not derive from the
  filename); the orchestrator spawns this agent via
  `subagent_type: "command-reviewer"`, which must match this field.
- `Edit` is intentionally excluded from the agent's `tools:` list
  (no source modification); `Write` is listed so the agent can
  produce the findings file. But the docs are explicit that
  `tools:` declares availability only, not a permission grant.
  The actual path-scoped permission is granted by the
  `Edit(/command-findings/**)` entry committed to
  `.claude/settings.local.json` (see Files section above). The
  rule uses `Edit(...)` because that scope covers all
  file-modification tools (`Write`, `Edit`, `NotebookEdit`); the
  fact that the agent's `tools:` list omits `Edit` is what
  prevents source modification, not the permission rule.
- `model: opus` -- maximum depth per command. With 17 fan-outs
  this is the expensive choice; accepted because the reviews are
  the artifact and depth matters more than throughput.
- `maxTurns` is omitted; the briefing's research budget (below)
  serves the same purpose with less risk of a premature stop.

System prompt body carries over from the current root-level prompt
briefing, with these edits:

- Drop "Read `AGENTS.md`. It's authoritative." -- auto-loaded via
  CLAUDE.md.
- Drop "Read `./btrfs-links.md` and..." [since removed from repo] preamble; replace with a
  pointer that says "If your command's tools include btrfs,
  cryptsetup/LUKS, systemd, smartctl, NUT, util-linux,
  autosuspend, hddfancontrol, or the kernel, consult
  `./btrfs-links.md` (selectively) and `./reference/` (vendored
  upstream source -- preferred over the web)."
- Add a **research budget** line: "Spend at most ~30% of your
  turns on external research. If you have not begun drafting
  findings by then, stop researching and write what you have."
- Add a **fallback rule** consistent with the user's global
  CLAUDE.md: "If `WebFetch` returns 403/429 or an anti-bot
  interstitial, skip that source -- do not block on it."
- Add an explicit **return-value contract** at the end: "Your
  final assistant message (which the orchestrator sees as the
  tool result) must be exactly:
  `Wrote ./command-findings/<slug>.md. Top finding: <one line>.`
  Do not echo the file contents."
- Keep the full findings file template (`# {COMMAND} review`,
  Scope, Findings with `(N)` numbering, Review coverage, Overall
  assessment).

## `prompts/command-review-fanout.md` shape (post-refactor)

Outline:

1. **Discovery:**
   - `rg -n '#\[command\(hide = true\)\]|^\s*[A-Z][A-Za-z]+\(' cli/src/main.rs`
     to enumerate variants and tag hidden ones.
   - `ls manual/commands/` to cross-check the documented surface.
   - Build the leaf manifest: 16 top-level + `ups status` = 17 leaf
     invocations. Use the effective Clap name (the `#[command(name
= "...")]` override if present; otherwise the kebab-cased
     variant). Concretely: `add`, `remove`, `remove-missing`,
     `replace`, `status`, `doctor`, `unlock`, `lock`, `enroll`,
     `idle`, `monitor`, `ack`, `tui`, `browse`, `discover`,
     `recover`, `ups status`. Slug for the findings file uses the
     same form with internal spaces hyphenated: `enroll.md`,
     `ups-status.md`, etc.
   - Reject hidden commands (`scrub-cancel`, `scrub-needs-resume`,
     `scrub-resume-or-start`).

2. **Pre-flight (verify-only -- abort if any check fails; do
   not mutate settings at runtime):**
   - `mkdir -p command-findings`. (Untracked directory; safe to
     create.)
   - Verify the `command-reviewer` agent loads:
     `claude agents | grep -q '^command-reviewer\b'` (or equivalent
     in the runtime). Abort if missing.
   - Verify the project-relative `Edit` permission rule is present
     in `.claude/settings.local.json` (or `.claude/settings.json`).
     The expected rule string is exactly:

     ```
     Edit(/command-findings/**)
     ```

     Check with:

     ```
     jq -e '.permissions.allow | index("Edit(/command-findings/**)")' \
       .claude/settings.local.json
     ```

     If absent, **abort the run** with a message instructing the
     user to add the rule (or to merge in the pending change to
     `.claude/settings.local.json` from this plan). The
     orchestrator must not mutate `settings.local.json` at runtime
     -- the file is tracked in git, and a mid-run mutation would
     break the "no modified tracked files" verification at step 7.

     Rationale: with `Write` listed in the agent's frontmatter but
     no matching `permissions.allow` rule, foreground subagents
     would each prompt the user 17 times, and background subagents
     would auto-deny. `Edit(/command-findings/**)` is the correct
     rule shape because `Edit(...)` scopes all file-modification
     tools (`Write`, `Edit`, `NotebookEdit`), and a single leading
     slash is project-relative per [permissions.md][4].

3. **Progress tracking (mandatory, user-visible):** `TaskCreate`
   accepts one task per call. The orchestrator must issue **18
   `TaskCreate` calls** (17 review + 1 rollup), each in `pending`,
   capturing the returned `taskId` for each. Subsequent
   `TaskUpdate` calls reference tasks by `taskId`.
   - 17 review tasks, one per leaf invocation, titled
     `Review braid <command>` (e.g. `Review braid status`,
     `Review braid ups status`).
   - 1 final rollup task titled
     `Roll up findings into command-findings/index.md`.

   The 18 `TaskCreate` calls may be issued as parallel `tool_use`
   blocks in a single assistant message; in-message ordering is
   preserved so the task list reads in the order they were issued.
   Maintain a local mapping `command -> taskId` for the subsequent
   `TaskUpdate` calls.

   Lifecycle rules the orchestrator must follow:
   - In the same assistant message that fans out the 17 `Agent`
     calls (step 4), also call `TaskUpdate` on each review
     `taskId` to flip it from `pending` to `in_progress`. The
     task list and the fan-out happen in one message so the user
     immediately sees "17 in progress, 0 complete".
   - As each subagent returns its one-line `Wrote
./command-findings/<slug>.md...` reply, call `TaskUpdate` on
     the matching `taskId` to `completed` before moving on. One
     `TaskUpdate` per return -- do not batch completions. This
     keeps the visible counter ("N of 17 complete") accurate in
     real time.
   - When all 17 review tasks are `completed`, flip the rollup
     `taskId` to `in_progress` and spawn the rollup agent. On its
     return, flip the rollup task to `completed`.
   - If a subagent fails or returns malformed output, mark its
     task `completed` with a short note in the task body (e.g.
     "returned malformed output -- see transcript"), do not leave
     it `in_progress`. The orchestrator can decide whether to
     retry that command in a follow-up message.

   Rationale: with 17 parallel subagents, the user has no way to
   see fan-out progress from the assistant's text output alone.
   The task list is the only durable, glanceable progress signal
   -- it must be wired into the actual spawn/return cycle, not
   added as an afterthought. Setting `in_progress` _with_ the
   fan-out (same message) is what makes the count truthful from
   the first second.

4. **Fan-out:** Send all 17 `Agent` calls in one assistant message
   as parallel `tool_use` blocks, alongside the `TaskUpdate` calls
   described in step 3. Each `Agent` call uses
   `subagent_type: "command-reviewer"` and a tiny prompt:

   ```
   Command: {COMMAND}
   Representative starting files:
   {FILES}
   ```

   The orchestrator picks `{FILES}` per command -- typically
   `cli/src/main.rs`, the matching `cli/src/<module>.rs` or command
   directory, the `manual/commands/<slug>.md` page, and any obvious
   shared planner/executor it touches. The reviewer is told to
   treat these as starting points, not the boundary.

5. **Concurrency note:** docs don't specify a parallel cap. Send
   all 17 in one message; if the harness caps the batch, split the
   remainder into a follow-up message.

6. **After all returns:** verify with `ls command-findings/ | wc
-l` (should be 17 before rollup) and `git status` (should show
   only untracked files under `command-findings/`).

7. **Rollup:** once all 17 review tasks are `completed`, flip the
   rollup `taskId` to `in_progress` and spawn one final
   `general-purpose` subagent to read all 17 files in
   `command-findings/` and emit `command-findings/index.md` -- a
   single table severity-sorted across all commands (High first,
   then Medium, then Low), each row linking back to the per-command
   file and finding number. Briefing for this rollup agent must
   explicitly forbid creating new findings or re-interpreting
   existing ones -- copy the one-line `Issue` text verbatim. Return
   value: one line, same contract as the per-command reviewers. On
   return, flip the rollup task to `completed`.

## Docs references that informed this plan

- **Custom subagents, frontmatter fields, fresh context window:**
  [`sub-agents.md`][1] -- "Each subagent runs in its own context
  window with a custom system prompt, specific tool access, and
  independent permissions." Project subagents live in
  `.claude/agents/` and are checked into version control.
- **Working directory + permission inheritance:**
  [`tools-reference.md`][2] -- subagent sessions do not carry over
  cwd changes; foreground subagents prompt for permissions the
  same as the main conversation.
- **Context model:** [`how-claude-code-works.md`][3] -- confirms
  parent conversation history is not passed to subagents; the
  prompt string is the only channel.

[1]: https://code.claude.com/docs/en/sub-agents.md
[2]: https://code.claude.com/docs/en/tools-reference.md
[3]: https://code.claude.com/docs/en/how-claude-code-works.md
[4]: https://code.claude.com/docs/en/permissions.md

## Verification

End-to-end:

1. Start a fresh Claude Code session in `/Users/dan/Code/braid`
   and feed the new `prompts/command-review-fanout.md`.
2. Confirm the pre-flight:
   - `command-findings/` exists.
   - `command-reviewer` agent loads (`claude agents` lists it).
   - `Edit(/command-findings/**)` is in
     `.claude/settings.local.json` (or `settings.json`).
   - Orchestrator aborts cleanly if any of the above are missing,
     rather than silently fanning out into denials/prompts. It
     does **not** attempt to mutate settings at runtime.
3. Confirm the orchestrator:
   - Issues 18 `TaskCreate` calls (17 review + 1 rollup), all
     `pending`. The 17 review tasks may be issued as parallel
     `tool_use` blocks in a single assistant message.
   - In the same assistant message as the fan-out, flips all 17
     review tasks to `in_progress` by `taskId`.
   - Issues a single assistant message containing 17 parallel
     `Agent` calls with `subagent_type: "command-reviewer"`.
4. Watch the task list during the run:
   - Each subagent return triggers exactly one `TaskUpdate` to
     `completed` -- counter advances one at a time, not in bulk.
   - Rollup task stays `pending` until all 17 review tasks are
     `completed`, then flips to `in_progress`, then `completed`.
5. Confirm each subagent reply (visible in the transcript) is the
   one-line `Wrote ./command-findings/<slug>.md. Top finding: ...`
   form -- not multi-paragraph prose.
6. `ls command-findings/` shows 17 per-command files plus
   `index.md` (18 total) with expected slugs (including `enroll.md`
   and `ups-status.md`; no `enroll-key-file.md` and no `ups.md`).
7. `git status` shows no modified tracked files -- only the new
   untracked findings dir.
8. Spot-check 2-3 findings files: each follows the template
   (Scope, Findings with `(N)` numbering, Review coverage, Overall
   assessment).
9. Open `command-findings/index.md`: rows are severity-sorted (all
   High before any Medium, all Medium before any Low), each row
   links back to a per-command file and a `(N)` anchor, and no
   rows contain text not present in the source findings file (no
   fabricated entries).

Smoke test before the full fan-out:

- Temporarily edit `prompts/command-review-fanout.md` to fan out a
  single command (e.g. `status`), run it, inspect
  `command-findings/status.md` for template fidelity and
  return-value brevity. Revert and run the full 17.

## Out of scope

- Path-level `Write` restriction on the subagent. The docs'
  frontmatter `tools` field is per-tool, not per-path. The path
  scope is enforced via the `permissions.allow` glob, not via the
  agent definition.
- Migrating any other prompts in the repo to the same pattern.

## Sources

Docs consulted during planning and this revision:

- [`sub-agents.md`](https://code.claude.com/docs/en/sub-agents) --
  custom subagent definition; required frontmatter fields (`name`,
  `description`); fresh-context-window model.
- [`tools-reference.md`](https://code.claude.com/docs/en/tools-reference)
  -- subagent permission/tool semantics (`tools` declares
  availability, not grant; background subagents auto-deny prompts;
  foreground subagents prompt); `TaskCreate` / `TaskUpdate` /
  `TaskList` shapes (single-task-per-call, `taskId` handle).
- [`how-claude-code-works.md`](https://code.claude.com/docs/en/how-claude-code-works)
  -- parent conversation history is not passed to subagents; the
  prompt string is the only channel.
- [`permissions.md`](https://code.claude.com/docs/en/permissions)
  -- `Edit(...)` rules cover all file-modification tools (`Write`,
  `Edit`, `NotebookEdit`); single leading slash is
  project-relative, double leading slash is filesystem-absolute.
- `cli/src/main.rs:23-88` -- canonical source for the `Commands`
  and `UpsCommand` enums; confirms `EnrollKeyFile` is exposed as
  `enroll` and `Ups` is parent-only.
- `.claude/settings.local.json` -- tracked file (`git ls-files`
  matches); existing `permissions.allow` set has no
  `Edit(/command-findings/**)` rule, which is the gap this plan
  closes via a committed edit, not a runtime mutation.
