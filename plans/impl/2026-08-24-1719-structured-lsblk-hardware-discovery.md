# Refactor disk hardware discovery onto structured lsblk JSON

## Problem and decision

Braid reads disk model, serial, and size through two contracts: fixture-covered
`lsblk --json` for the TUI, and unstructured one-field subprocesses for
confirmation prompts, status, and replacement-size preflight. The scalar path
spawns two or three processes per disk and falls outside the stable util-linux
contract established by ADR 010.

Use one structured, device-scoped JSON query per disk for those callers. Keep
the existing whole-system query for TUI discovery. Device-scoped queries retain
device-level failure isolation; command-wide batching is rejected because
util-linux exits 64 when only some requested devices exist, while braid's JSON
parser rejects every nonzero exit.

## Invariants

- Both query forms use the same explicit JSON column contract.
- A device query excludes dependencies and resolves exactly one top-level
  device; any other result shape is unavailable metadata.
- Model and serial retain the current normalization: surrounding whitespace is
  trimmed and an empty value is unavailable.
- Confirmation prompts and status remain best-effort when hardware metadata
  cannot be queried or parsed.
- Replacement sizing remains fail-closed before mutation when SIZE is
  unavailable.
- Present disks are still queried through live backing paths; add and
  replacement targets still use their by-id paths.
- Queries are neither batched across disks nor cached across planning and
  execution.

## Proof obligations

- Pin the whole-system and device-scoped command shapes, including one process
  per device and the device-only dependency scope.
- Cover childless JSON, nullable fields, text normalization, command and parse
  failure, and zero or multiple top-level devices.
- Preserve path-sensitive confirmation and status coverage.
- Preserve replacement preflight's refusal when SIZE cannot be established.
- Remove the scalar command surface and its mocks completely.

## Delivery

Land the command surface, callers, tests, fixtures, and TASK-21 tracker update
as one coherent commit. No ADR or user-documentation change is required because
observable behavior and the documented util-linux contract do not change.
