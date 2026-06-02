# Command Review Fan-Out

Discover the public command surface before spawning reviewers: run
`rg -n '#\[command\(hide = true\)\]|^\s*[A-Z][A-Za-z]+\(' cli/src/main.rs`
and `ls docs/commands/`. Build the leaf manifest from `Commands` and
public nested subcommand enums, using effective Clap names. Expected
review targets: `add`, `remove`, `remove-missing`, `replace`, `status`,
`doctor`, `unlock`, `lock`, `enroll`, `idle`, `monitor`, `ack`, `tui`,
`discover`, `recover`, and `ups status`. Exclude hidden scrub
helpers and do not review `ups` as a parent-only command.

Pre-flight is verify-only except for the findings directory:

```sh
mkdir -p command-findings
test -f .claude/agents/command-reviewer.md
jq empty .claude/settings.json
```

Abort if the agent file is missing or the committed shared settings file
is invalid. The caller must already have local mutating permissions for
`command-findings/`; do not mutate Claude settings at runtime.

Create user-visible progress tasks before fan-out: issue 17
`TaskCreate` calls in `pending` state, one titled `Review braid
<command>` for each of the 16 targets and one titled `Roll up findings
into command-findings/index.md`. Capture every `taskId` and maintain a
local `command -> taskId` mapping. In the same assistant message that
fans out the reviewers, call `TaskUpdate` on each review task to mark
it `in_progress`.

Spawn all 16 reviewers as parallel `Agent` tool calls with
`subagent_type: "command-reviewer"`. If the harness caps parallelism,
send the remainder in a follow-up message. Each prompt should be only:

```text
Command: braid {COMMAND}
Representative starting files:
{FILES}
```

Choose `{FILES}` as useful starting points for that command, typically
`cli/src/main.rs`, the matching `cli/src/<module>.rs` or command
directory, any obvious shared planner/executor module, and
`docs/commands/<slug>.md`. These are starting points only; the
subagent owns full discovery.

As each reviewer returns
`Wrote ./command-findings/<slug>.md. Top finding: <one line>.`, call
`TaskUpdate` for that command's task to
mark it `completed` before processing the next return. If a subagent
fails or returns malformed output, still mark its task `completed` with
a short note in the task body.

After all 16 review tasks are completed, verify
`ls command-findings/ | wc -l` reports 16 and `git status` shows only
untracked files under `command-findings/`. Then mark the rollup task
`in_progress` and spawn one `general-purpose` rollup agent to read the
16 findings files and write `command-findings/index.md`. The rollup may
not create new findings or reinterpret existing ones; copy each
one-line `Issue` text verbatim into a severity-sorted table, High
before Medium before Low.

Columns, left to right: `Status | Severity | Command | Finding | Issue`.

- `Status` initializes to a single space `" "` for every row.
- `Severity` is `High`, `Medium`, or `Low`, copied from the source
  finding.
- `Command` is the braid command name (e.g. `add`, `ups status`).
- `Finding` links to the per-command file at the source finding's
  line, formatted `[#N](./<slug>.md:<lineno>)`. `<lineno>` is the line
  number in `command-findings/<slug>.md` of the standalone line
  matching `^\(N\)$` -- the line that introduces finding `(N)`. For
  example, if `(3)` is on line 147 of `add.md`, the cell is
  `[#3](./add.md:147)`.
- `Issue` is the one-line `Issue:` text from the source finding,
  verbatim.

When the rollup returns its one-line
`Wrote ./command-findings/index.md. Top finding: <one line>.` result,
mark the rollup task `completed`.

## Extra focus

Append this section verbatim to every subagent prompt. Delete the body
for a default run.

braid just migrated disk identity to LUKS UUIDs. Guidelines:
`docs/design/decisions/024-luks-uuid-identity.md`. Flag code, tests, fixtures,
docs, error messages, or comments that still use pre-migration
identifiers (device paths, serials, by-id) or otherwise contradict that
doc.
