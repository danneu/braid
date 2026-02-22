# Proposal: Disk Identity Model (Config vs Runtime)

## Goal
Define a disk identity model that removes mapper-name alias bugs (for example `disk1` vs `virtio-disk1`) while keeping operator config simple and deterministic.

## Core Position
1. **Config identity:** `/dev/disk/by-id/...` remains the operator-facing source of truth.
2. **Runtime identity:** LUKS UUID (and dm UUID as fallback) is used for matching and reconciliation.
3. **Execution handle:** `/dev/mapper/<name>` is treated as ephemeral and never as canonical identity.

## Why
1. `/dev/disk/by-id/...` is stable and human-auditable in config.
2. Mapper names are implementation detail and can differ across boot/open paths.
3. UUID-backed matching eliminates string-alias edge cases and reduces planner complexity.
4. Keeps behavior aligned with current principles: stable identifiers + safe-by-construction apply.

## Identity Layers
1. **DeclaredDisk**
   - `declared_by_id: String` (from `braid.disks`)
   - optional metadata: model/serial (display only)
2. **EncryptedDevice**
   - `luks_uuid: String` (canonical runtime identity)
   - `dm_uuid: Option<String>` (fallback)
3. **OpenMapping**
   - `mapper_name: String`
   - `mapper_path: /dev/mapper/<name>`
   - strictly operational, not identity
4. **PoolMembership**
   - btrfs devid/path records joined back to `EncryptedDevice` identity

## Matching Rules
1. Resolve declared by-id path to block device.
2. Read LUKS UUID from block device.
3. Enumerate open dm-crypt mappings and their UUIDs.
4. Match by UUID, not by mapper-name equality.
5. If UUID match exists with non-preferred mapper name, treat as same disk and continue safely.

## Command Contracts
1. `init-disk`
   - Accepts declared by-id path.
   - Formats only here.
   - After format, records/prints UUID-derived identity in status output.
2. `plan`
   - Computes add/remove/replace using UUID identity joins.
   - No-op when disk is already represented under an alias mapper name.
3. `apply`
   - Executes against mapper paths, but selection is UUID-resolved.
   - Never uses mapper-name equality as gate for identity checks.
4. `status`
   - Shows both `by_id` and `luks_uuid`.
   - Shows mapper name as runtime detail only.

## Data Model Sketch
```text
DiskRef {
  by_id: String,
  luks_uuid: Option<String>,
  dm_uuid: Option<String>,
  mapper_name: Option<String>,
  mapper_path: Option<String>,
  btrfs_devid: Option<u64>,
  present: bool,
}
```

## Simplicity Constraints
1. No persistent registry database.
2. No mutable mapping table between by-id and mapper names.
3. Derive everything from live probes + declared config each run.
4. Fail closed when identity cannot be established unambiguously.

## Failure Behavior
1. **By-id missing:** mark absent, skip destructive operations, emit warning.
2. **UUID unreadable on declared disk:** block relevant actions; explicit operator error.
3. **UUID collision/anomaly:** block plan/apply with high-severity diagnostic.
4. **Mapper exists without matching declared UUID:** treat as unmanaged/open foreign device.

## Test Strategy (Robustness + Correctness)
### Unit/property tests
1. Alias mapper names map to same disk via UUID.
2. Planner idempotence under renaming of mapper names.
3. No plan/apply identity decision depends solely on mapper string.

### Integration tests (mocked probe/exec)
1. Resume where mapper name changed but UUID is same.
2. Mixed environment with unmanaged dm-crypt mapping present.
3. Replace flow with unplug/replug and mapper alias drift.

### VM tests
1. Existing full VM suite retained.
2. Add explicit regression: boot/open with short mapper aliases while config uses by-id basename.
3. Add explicit regression: apply no-op when same UUID appears under different mapper name.

## Migration Plan
1. Introduce UUID fields in probe output and internal structs.
2. Switch planner joins from mapper-name matching to UUID matching.
3. Keep deterministic mapper naming for opens/closes, but demote to execution-only detail.
4. Update `status --json` schema with explicit identity fields (`by_id`, `luks_uuid`, `mapper_name`).
5. Add migration tests before removing legacy mapper-name assumptions.

## Acceptance Criteria
1. No false add/remove actions when mapper alias differs.
2. All identity-sensitive decisions are UUID-backed.
3. Existing safety gates remain intact (`init-disk` only formatting path, apply non-destructive).
4. VM and fast tests both pass, including new alias regressions.
