Dream end state:

1. braid plan (dry-run)

- Reads Nix config + live state.
- Shows exactly what will happen (add, remove, replace, degraded risk, capacity
  impact).

2. braid apply

- Executes a generated plan with checkpoints and resumability.
- Internally uses the same safe primitives (device remove, remove missing, add,
  rebalance).

3. First-class braid replace-disk <old> <new>

- Best UX for common failure recovery.
- Handles dead/present old disk automatically, updates progress, and verifies
  healthy end state.

4. braid-status with machine-readable output

- Human summary by default, --verbose, and --json for automation.

5. Keep Nix as source of truth

- Still config-first (edit config -> rebuild -> braid apply), with tooling that
  makes convergence obvious and hard to misuse.

So: long-term I’d move from “single-purpose scripts” to a plan/apply state
machine CLI (and later TUI), with replace-disk as a first-class workflow.

You can do it now. The reason to stage it is risk and sequencing, not capability.

1. braid-remove-disk is missing, and that is a core safety primitive.
2. A plan/apply engine depends on solid primitives first (add, remove, status),
   otherwise it wraps unstable behavior.
3. Given your principles, each behavior change needs tests + decision/docs
   updates, so “do everything now” is a larger blast radius.

If you want to move fast without overextending, best sequence is:

1. Ship braid-remove-disk with the decisions you just made.
2. Ship braid-status (summary + verbose).
3. Add braid-replace-disk (or keep replace in add with clearer UX).
4. Then build plan/apply on top of proven commands.

---

---

---

Proposed Plan
Goal: move from one-off scripts to a single, safe, config-first disk workflow
without breaking current behavior.

1. Phase 1: Solid primitives (now)

- Implement braid-remove-disk with the agreed guards and confirmations.
- Implement braid-status with default summary + --verbose.
- Keep current braid-add-disk replace behavior (auto-evict missing).
- Justification: plan/apply is only trustworthy if low-level operations are
  already safe and test-backed.

2. Phase 2: Read-only planner

- Add braid plan that compares:
  - desired state (/etc/braid/config.json)
  - live state (btrfs, mapper devices, missing devices, mount health)
- Print planned actions: add, remove, replace, risk flags, and capacity/
  redundancy impact.
- Justification: users see intent before mutation; this is the Nix-native
  “preview drift and converge” step.

3. Phase 3: Executor with checkpoints

- Add braid apply that executes a previously computed plan.
- Persist checkpoints so interrupted runs can resume safely.
- Use the same internal primitives as scripts (btrfs device remove, remove
  missing, add+rebalance, close luks).
- Justification: resumability and explicit step boundaries reduce recovery
  complexity after reboot/failure.

4. Phase 4: First-class replace

- Add braid replace-disk <old> <new>.
- Detect old disk present vs dead and choose graceful remove vs missing remove
  automatically.
- Verify end-state: no unexpected missing devices, expected disk count, RAID
  profile intact.
- Justification: replace is the highest-stress operator flow and deserves a
  dedicated command.

5. Phase 5: Automation interface

- Extend braid-status with --json.
- Keep human-friendly default and --verbose.
- Justification: enables monitoring, dashboards, and future TUI without scraping
  text output.

Why this is the right shape

- Preserves your core invariant: nixos-rebuild stays non-destructive.
- Keeps Nix config authoritative while still handling inherently imperative
  storage mutation.
- Gives users one mental model: edit config -> rebuild -> plan -> apply.
- Improves UX and safety incrementally instead of a risky “big-bang” rewrite.

README Pitch: ## Disk management (draft)

## Disk management

Braid is config-first: declare desired disks in NixOS, rebuild, then let braid
converge live storage.

### Workflow

1. Edit `braid.disks` in your NixOS config.
2. Run `sudo nixos-rebuild switch`.
3. Preview changes:

   ```bash
   sudo braid plan

   ```

4. Apply changes:

   sudo braid apply

braid plan shows exactly what will happen (add, remove, replace), including
redundancy and capacity impact before anything mutates.

### Common operations

Add a disk:

# 1) add disk path to braid.disks

sudo nixos-rebuild switch
sudo braid plan
sudo braid apply

Remove a disk:

# 1) remove disk path from braid.disks

sudo nixos-rebuild switch
sudo braid plan
sudo braid apply

Replace a failed disk:

# 1) remove dead disk from braid.disks, add replacement disk path

sudo nixos-rebuild switch
sudo braid replace-disk /dev/disk/by-id/<old> /dev/disk/by-id/<new>

### Status

Quick health summary:

sudo braid-status

Detailed per-disk diagnostics:

sudo braid-status --verbose

Machine-readable output:

sudo braid-status --json

---

---
