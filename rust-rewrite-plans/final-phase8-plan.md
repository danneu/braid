# Phase 8: Cleanup + Docs

## Context

Phase 7 made the Rust binary the primary `braid` command. The codebase still has dead bash scripts, stale doc references to the bash era, and planning artifacts cluttering the repo root. This phase cleans all of that up.

No code changes — purely file moves, deletions, and doc edits.

---

## 1. Delete dead bash scripts

`scripts/braid.sh` (1640 lines) and `scripts/braid-status.sh` (182 lines) are dead code — no test, module, or flake reference. Delete them.

**Keep:** `scripts/braid-remove-disk.sh` (still used by tests 9 and 12) and `scripts/braid-add-disk.sh` (error stub).

| Action | File |
|--------|------|
| Delete | `scripts/braid.sh` |
| Delete | `scripts/braid-status.sh` |

---

## 2. Archive planning docs

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

Also move `brainstorm/` → `archive/brainstorm/`.

---

## 3. Update user-facing docs

### `docs/1-user-stories.md` (lines 31–47)

Replace the `braid-add-disk` step 4 with `init-disk` + `apply`:

```
4. Format and add it to the pool:
   ```
   $ sudo braid init-disk /dev/disk/by-id/ata-Toshiba_MN07_XXXX
   $ sudo braid apply
   ```
```

The init-disk + plan example for the second disk (lines 80–92) is already correct.

### `README.md` (lines 151–159)

Remove the "Standalone scripts (deprecated)" section entirely. `braid-add-disk` is an error stub, `braid-status` is deleted, `braid-remove-disk` is a legacy script that isn't user-facing documentation.

### `docs/decisions/unified-cli.md`

Update to reflect Rust reality:
- **Line 9**: "Braid has three standalone scripts" → past tense, note Rust CLI replaced them
- **Lines 21–32**: Decision section says "Bash+jq" — add a note that this was the initial implementation, now replaced by Rust CLI
- **Line 74**: Backward compat section — update: `braid-add-disk` is now an error stub (not a wrapper), `braid-status` removed, `braid-remove-disk` remains as standalone legacy script
- **Lines 85–86**: Update "See" references — `scripts/braid.sh` deleted, note it

### `docs/decisions/toolchain-pinning.md`

- **Line 7**: "parsed by both the shell script and the Rust CLI" → "parsed by the Rust CLI"
- **Line 11**: "Both the shell and Rust wrappers" → "The Rust wrapper"
- **Line 17**: Remove `writeShellApplication` reference, just mention `makeWrapper`
- **Line 18**: Already correct (updated in Phase 7)

---

## 4. Update AGENTS.md references

- **Line 42**: `design-docs/1-braid-add-disk.md` reference — already says "(historical, replaced by unified CLI)" — leave as-is
- **Lines 43**: `design-docs/3-daemon.md` — still future work, leave as-is

No changes needed.

---

## Files modified

| File | Change |
|------|--------|
| `scripts/braid.sh` | Delete |
| `scripts/braid-status.sh` | Delete |
| `docs/1-user-stories.md` | Replace `braid-add-disk` example with `init-disk` + `apply` |
| `README.md` | Remove "Standalone scripts (deprecated)" section |
| `docs/decisions/unified-cli.md` | Update to reflect Rust is now the only CLI |
| `docs/decisions/toolchain-pinning.md` | Remove shell/bash references |
| `archive/proposals/` | New dir — move 8 root-level planning .md files |
| `archive/rust-rewrite-plans/` | Move from `rust-rewrite-plans/` |
| `archive/brainstorm/` | Move from `brainstorm/` |

**NOT modified:** `scripts/braid-remove-disk.sh` (still tested), `scripts/braid-add-disk.sh` (error stub, harmless), `AGENTS.md` (already accurate).

---

## Verification

1. `grep -r "braid\.sh" modules/ flake.nix tests/` — zero matches (no active code references deleted script)
2. `grep -r "braid-status\.sh" modules/ flake.nix tests/` — zero matches
3. `ls scripts/` — only `braid-add-disk.sh` and `braid-remove-disk.sh` remain
4. `ls *.md` (root) — only `AGENTS.md`, `CLAUDE.md`, `README.md` remain
5. `nix flake show` — evaluates cleanly (no broken imports)
6. `make test-one t=braid-remove-disk` — still passes (script not deleted)
7. `make test-one t=braid-unified` — still passes (uses braid-remove-disk.sh)
