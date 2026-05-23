# Split Claude Code settings and remove auto memory

## Summary

Use Claude Code's documented settings split:

- `.claude/settings.json` is committed and shared with the repo.
- `.claude/settings.local.json` is ignored and holds personal, mutating, or machine-specific settings.

Disable auto memory at the project level and remove the tracked `.claude/memory/`
corpus. Official references:

- https://code.claude.com/docs/en/settings
- https://code.claude.com/docs/en/memory

## Key changes

- Add committed `.claude/settings.json`:

  ```json
  {
    "$schema": "https://json.schemastore.org/claude-code-settings.json",
    "autoMemoryEnabled": false,
    "permissions": {
      "allow": [
        "Bash(nix log:*)",
        "Bash(grep:*)",
        "Bash(head:*)",
        "Bash(tail:*)",
        "Bash(modinfo:*)",
        "Bash(zstdcat:*)",
        "Bash(git ls-remote *)",
        "Bash(cargo tree *)",
        "Bash(nix --version)",
        "Bash(just --list)",
        "WebSearch"
      ]
    }
  }
  ```

- Stop tracking `.claude/settings.local.json`; keep the local file in the working
  tree as ignored personal state.
- Remove `autoMemoryDirectory` from the local settings file. Claude Code docs say
  `autoMemoryDirectory` is not accepted from project or local settings, so it is
  not the right control for this repo.
- Keep these permissions local because they are mutating, account/auth-dependent,
  browser-interactive, or machine-specific:
  - tests/builds/checks
  - `git add`, `git commit`, `git mv`
  - `mkdir`
  - `Edit(...)`
  - `Read(//tmp/**)` and `Read(//Users/dan/**)`
  - `gh ...`
  - Playwright tools
  - DanTerm tools
  - `Skill(revise-plan)`
- Remove tracked `.claude/memory/` files entirely.
- Update `.gitignore` to ignore:
  - `/.claude/settings.local.json`
  - `/.claude/memory/`
  - `/.claude/worktrees/`
  - `.DS_Store`

## Repo gate cleanup

- Edit `scripts/docs/check-code-doc-anchors.py`: remove the
  `ROOT / ".claude/memory"` entry from `SEARCH_ROOTS` so the gate no
  longer treats the deleted directory as a source root for principle
  anchor citations.

## Prompt cleanup

- Update `prompts/command-review-fanout.md` so its permission preflight no longer
  requires `.claude/settings.local.json`.
- If the preflight still checks permissions, it must only check committed shared
  settings in `.claude/settings.json`; any requirement for local mutating
  permissions should be described as a caller prerequisite, not enforced by
  reading ignored local state.
- Leave historical plan files alone even if they mention old Claude settings.

## Test plan

Each step states the expected post-state explicitly so we are checking the
cleanup, not pre-existing global state.

**Pre-implementation snapshot.** Before touching the local file, the
implementer must snapshot it so the post-state can be diffed against it.
`.claude/settings.local.json` becomes untracked and ignored, so Git
cannot catch a clobber of its contents -- the snapshot is the only
reference point.

```sh
cp .claude/settings.local.json /tmp/braid-local-before.json
```

- `jq empty .claude/settings.json` -- exits 0 (valid JSON).
- `test -f .claude/settings.local.json` -- exits 0. The local file must
  still exist in the working tree after implementation; the plan
  preserves it as ignored personal state, it does not delete it.
- `jq empty .claude/settings.local.json` -- exits 0 (valid JSON).
- `jq -e 'has("autoMemoryDirectory") | not' .claude/settings.local.json`
  -- exits 0. Proves the `autoMemoryDirectory` key was actually removed
  from the local file. `git grep` cannot verify this once the file is
  untracked and ignored, so this jq check is the only thing that
  catches a stale `autoMemoryDirectory` line in the local file.
- Local-file preservation diff:

  ```sh
  diff -u \
    <(jq -S 'del(.autoMemoryDirectory)' /tmp/braid-local-before.json) \
    <(jq -S . .claude/settings.local.json)
  ```

  -- exits 0 (no diff). Proves the implementation removed exactly
  `autoMemoryDirectory` and nothing else from the local file: no
  permissions dropped, no other keys rewritten, no minimal-JSON
  clobber. Canonical (`-S`) JSON on both sides so key order and
  whitespace do not affect the comparison.
- `git ls-files .claude/settings.local.json .claude/memory` -- prints nothing
  (neither path is tracked).
- `git check-ignore -v .claude/settings.local.json .claude/memory/MEMORY.md
  .claude/worktrees/example` -- every line's source column must be the
  project `.gitignore` (e.g. `.gitignore:N:/.claude/...`), proving the new
  project rules are doing the ignoring, not a global ignore file. The
  `.claude/settings.local.json` line in particular proves the file is
  ignored by the project rule we are adding, not by an ad-hoc
  per-worktree exclude.
- `git check-ignore -v .DS_Store` -- source column must be `.gitignore`
  (the project file), not `/Users/dan/.config/git/ignore`. This is the
  step that proves the project `.gitignore` change took effect; relying
  on plain `git check-ignore` here would pass via the user's global
  ignore even if the project rule were missing.
- `git grep -n "autoMemoryDirectory\\|\\.claude/settings.local.json\\|\\.claude/memory" -- . ':(exclude)plans/**' ':(exclude).gitignore'`
  -- prints nothing. `.gitignore` is excluded because the plan
  intentionally adds matching entries there; any other hit is a missed
  cleanup site.
- `git diff --check` -- exits 0 (no whitespace errors).

## Assumptions

- Existing auto-memory files are not intentionally curated project documentation;
  removing them is part of disabling auto memory for public release.
- Shared read-only permissions are the non-mutating rules that do not contain
  personal absolute paths or account-specific write privileges.
- Historical `plans/**` references can remain until a separate public-release
  cleanup removes or archives planning history.

## Implementation notes

- Used the current unstaged `.claude/settings.local.json` as the preservation
  baseline after explicit user approval, so existing local permission additions
  were retained while removing only `autoMemoryDirectory`.
- Audited the staged `.claude/memory/` deletions before commit; durable,
  broadly applicable guidance was folded into `AGENTS.md` and
  `docs/dev/testing.md`, while personal context, index files, and
  over-specific anecdotes remained deleted.
