# Phase 8: Cleanup + Docs

## Context

Phase 7 made the Rust binary the primary `braid` command. The codebase still has dead bash scripts, stale doc references to the bash era, and planning artifacts cluttering the repo root. This phase cleans all of that up.

No code changes — purely file moves, deletions, and doc edits.

---

## 1. Delete dead bash scripts

`scripts/braid.sh` (1640 lines) and `scripts/braid-status.sh` (182 lines) are dead code — no test, module, or flake references them. Delete them. Git history preserves them if needed.

**Keep:** `scripts/braid-remove-disk.sh` (still used by tests 9 and 12) and `scripts/braid-add-disk.sh` (error stub).

| Action | File |
|--------|------|
| Delete | `scripts/braid.sh` |
| Delete | `scripts/braid-status.sh` |

---

## 2. Archive retired bash tests

Tests 8, 10, 11, 14 are retired from `checksFor` (Phase 7 commit 2) but their `.nix` + `.py` files still live in `tests/` and reference `scripts/braid.sh`. Move them to `archive/tests-legacy/` so the `tests/` tree is clean and verification greps don't need exceptions.

| Source | Destination |
|--------|-------------|
| `tests/8-braid-status.nix` | `archive/tests-legacy/` |
| `tests/braid-status.py` | `archive/tests-legacy/` |
| `tests/10-braid-plan.nix` | `archive/tests-legacy/` |
| `tests/braid-plan.py` | `archive/tests-legacy/` |
| `tests/11-braid-apply.nix` | `archive/tests-legacy/` |
| `tests/braid-apply.py` | `archive/tests-legacy/` |
| `tests/14-braid-init-disk.nix` | `archive/tests-legacy/` |
| `tests/braid-init-disk.py` | `archive/tests-legacy/` |

---

## 3. Archive planning docs (root-level, rust-rewrite-plans, brainstorm)

### Root-level proposals → `archive/proposals/`

```
claude-proposal1.md
codex-plan-apply-state-matrix.md
codex-proposal-disk-id.md
codex-proposal-rust-cli.md
codex-proposal-rust-with-daemon.md
codex-proposal1.md
final-proposal.md
plan.md
```

### Rust rewrite plans → `archive/rust-rewrite-plans/`

Move entire `rust-rewrite-plans/` directory (including `plan-command-progress.md` which is untracked).

### Brainstorm → `archive/brainstorm/`

Move entire `brainstorm/` directory.

---

## 4. Update user-facing docs

### `docs/1-user-stories.md` (lines 31–47)

Replace the `braid-add-disk` step 4 with the `init-disk` + `apply` workflow:

```markdown
4. Format and add it to the pool:
   ```
   $ sudo braid init-disk /dev/disk/by-id/ata-Toshiba_MN07_XXXX
   $ sudo braid apply
   ```
```

The init-disk + plan example for the second disk (lines 80–92) is already correct.

### `README.md` (lines 151–159)

Remove the "Standalone scripts (deprecated)" section entirely. The scripts listed there are either deleted (`braid-status`), an error stub (`braid-add-disk`), or a legacy script not worth documenting (`braid-remove-disk`).

---

## 5. Update decision docs

Four decision docs reference `scripts/braid.sh` or contain stale bash-era language. All must be updated since we're deleting that file.

### `docs/decisions/unified-cli.md`

- **Line 9**: "Braid has three standalone scripts" → past tense, note Rust CLI replaced them all
- **Line 21**: "Option 3. Bash+jq..." → add note: "Initial implementation was bash+jq. Now replaced by Rust CLI (`cli/`)."
- **Line 25**: "Single script `scripts/braid.sh`" → "Originally `scripts/braid.sh`, now `cli/src/` (Rust)"
- **Line 32**: "Packaged via `pkgs.writeShellApplication`..." → "Now packaged via Crane + `makeWrapper` in `flake.nix`."
- **Line 74**: Backward compat section → Update: `braid-add-disk` is now an error stub (not a wrapper); `braid-status` is deleted (replaced by `braid status`); `braid-remove-disk` remains as standalone legacy script (not yet ported)
- **Line 85**: `scripts/braid.sh` → `cli/src/` (Rust CLI)
- **Line 86**: `scripts/braid-add-disk.sh` → note it's an error stub

### `docs/decisions/config-first-workflow.md`

- **Line 58**: `scripts/braid.sh` → `cli/src/` (Rust CLI: `init-disk`, `plan`, `apply`, `status`)
- **Line 59**: `scripts/braid-add-disk.sh` → note it's an error stub (not a wrapper)

### `docs/decisions/safe-by-construction-reconciliation.md`

- **Line 60**: `scripts/braid.sh` → `cli/src/` (Rust CLI: `init-disk`, `plan`, `apply`, `status`)

### `docs/decisions/toolchain-pinning.md`

- **Line 7**: "parsed by both the shell script and the Rust CLI" → "parsed by the Rust CLI"
- **Line 11**: "Both the shell and Rust wrappers" → "The Rust wrapper executes"
- **Line 17**: Remove `writeShellApplication (shell)` reference — just `makeWrapper` (Rust)
- **Line 18**: Already correct (updated in Phase 7)

---

## Files modified

| File | Change |
|------|--------|
| **Deletions** | |
| `scripts/braid.sh` | Delete |
| `scripts/braid-status.sh` | Delete |
| **Moves — retired tests** | |
| `tests/8-braid-status.nix` + `tests/braid-status.py` | → `archive/tests-legacy/` |
| `tests/10-braid-plan.nix` + `tests/braid-plan.py` | → `archive/tests-legacy/` |
| `tests/11-braid-apply.nix` + `tests/braid-apply.py` | → `archive/tests-legacy/` |
| `tests/14-braid-init-disk.nix` + `tests/braid-init-disk.py` | → `archive/tests-legacy/` |
| **Moves — planning docs** | |
| `claude-proposal1.md` | → `archive/proposals/` |
| `codex-plan-apply-state-matrix.md` | → `archive/proposals/` |
| `codex-proposal-disk-id.md` | → `archive/proposals/` |
| `codex-proposal-rust-cli.md` | → `archive/proposals/` |
| `codex-proposal-rust-with-daemon.md` | → `archive/proposals/` |
| `codex-proposal1.md` | → `archive/proposals/` |
| `final-proposal.md` | → `archive/proposals/` |
| `plan.md` | → `archive/proposals/` |
| `rust-rewrite-plans/` | → `archive/rust-rewrite-plans/` |
| `brainstorm/` | → `archive/brainstorm/` |
| **Doc updates** | |
| `docs/1-user-stories.md` | Replace `braid-add-disk` example with `init-disk` + `apply` |
| `README.md` | Remove "Standalone scripts (deprecated)" section |
| `docs/decisions/unified-cli.md` | Update for Rust-only CLI, fix stale script refs |
| `docs/decisions/config-first-workflow.md` | Update See section: `scripts/braid.sh` → `cli/src/` |
| `docs/decisions/safe-by-construction-reconciliation.md` | Update See section: `scripts/braid.sh` → `cli/src/` |
| `docs/decisions/toolchain-pinning.md` | Remove shell/bash wrapping references |

**NOT modified:** `scripts/braid-remove-disk.sh` (still tested), `scripts/braid-add-disk.sh` (error stub, harmless), `AGENTS.md` (already accurate), `tests/` (no changes).

---

## Verification

### Reference integrity — no broken links to deleted/moved files

```bash
# Deleted scripts not referenced from active docs, modules, tests, or flake
rg -n "scripts/braid\.sh|scripts/braid-status\.sh" docs/ README.md AGENTS.md modules/ flake.nix tests/
# Expected: zero matches

# Moved planning files not referenced from active docs
rg -n "claude-proposal1|codex-proposal|final-proposal\.md|plan\.md|rust-rewrite-plans|brainstorm/" docs/ README.md AGENTS.md modules/ flake.nix tests/
# Expected: zero matches
```

### Repo structure

```bash
# Only 2 scripts remain
ls scripts/
# Expected: braid-add-disk.sh  braid-remove-disk.sh

# Root-level .md files are clean
ls *.md
# Expected: AGENTS.md  CLAUDE.md  README.md

# No retired bash tests in tests/
ls tests/8-* tests/10-* tests/11-* tests/14-*
# Expected: not found (all moved to archive/tests-legacy/)
```

### Build + full test gate

```bash
nix flake show                      # evaluates cleanly (no broken imports)
make test-one t=braid-remove-disk   # braid-remove-disk.sh still works
make test-one t=braid-unified       # uses braid-remove-disk.sh, still works
make test                           # full suite passes — catches any regressions from deletes/moves
```
