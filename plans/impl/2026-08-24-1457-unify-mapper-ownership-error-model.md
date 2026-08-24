# Unify Mapper Ownership Error Modeling

## Problem

`classify_mapper_ownership` produces one set of mapper-ownership failures, but
`LuksError` and `ProbeError` each reconstruct the same three conditions with
duplicate fields and byte-identical operator messages. Their mirror-image
conversions can drift even though the classifier and one message fragment are
already shared.

The duplication is only partly protected: exact rendering tests cover the UUID
conflict and stale-mapper cases on both paths, while backing mismatch and
backing-path resolution have no exact parity lock.

## Decision

- Introduce a shared public `MapperOwnershipFailure` error type that owns the
  conflict, backing-path mismatch, and backing-path resolution conditions,
  including their fields and authoritative operator-facing rendering.
- Make the internal classifier error, `LuksError`, and `ProbeError` carry that
  typed failure without unpacking and rebuilding it. Keep command and parse
  failures at their existing layer-specific boundaries.
- Consumers that treat every ownership failure alike match the shared wrapper.
  Consumers that distinguish conditions match the nested failure exhaustively,
  preserving the compile-time requirement to classify future conditions.
- Keep replace's target-specific errors and wording separate. Its conversion
  from the shared failure remains exhaustive because replace supplies different
  context and remediation fields.

## Invariants

- **I1:** Every existing operator-reachable mapper-ownership failure renders
  byte-for-byte the same inner message as before, including punctuation and
  remediation commands.
- **I2:** Existing outer CLI attribution remains unchanged: command paths may
  still add `luks error:` or `probe error:`, while status and monitor may render
  the self-contained ownership message directly.
- **I3:** UUID conflict, stale mapper, resolved-path mismatch, and path-resolution
  failure remain distinguishable typed conditions; resolution failures preserve
  their `std::io::Error` source.
- **I4:** The TUI continues to classify UUID conflict and backing mismatch as a
  mapper conflict, while backing-path resolution failure remains unclassified
  rather than asserting a hijack.
- **I5:** Replace retains its existing target-specific error variants and
  operator messages.

## Proof Obligations

- **PO1 (I1, I3):** Exact rendering coverage proves the shared type's UUID-found,
  no-backing, backing-mismatch, and deterministic resolution-failure messages.
- **PO2 (I1, I2):** Rendering a shared failure through both `LuksError` and
  `ProbeError` proves that neither wrapper changes the inner message.
- **PO3 (I3, I4, I5):** Existing classifier, open, probe, status-advisory, add,
  replace, and TUI behavioral tests continue to prove condition selection and
  downstream classification after adopting the shared type.
- **PO4:** A tracked-file sweep finds no removed parallel ownership variants or
  duplicate generic message bodies; only the shared rendering and intentionally
  different replace wording remain.
- Run `cargo fmt --check`, `just test-rust`, `just clippy`, and
  `just check-output-ascii`.

## Non-goals

- No mapper-ownership behavior, remediation wording, subsystem prefix, ADR,
  README, or command documentation changes.
- No new VM scenario: existing VM coverage already exercises the backing-path
  refusal behavior, while this refactor is locked by focused Rust behavior tests.

## Implementation Discretion

- The shared type's exact placement within the LUKS module and the private
  formatting mechanism are left to implementation, provided there is one
  authoritative representation and rendering path.
- Test helper and table organization are left to implementation; assertions
  must prove the behavioral obligations above rather than internal structure.

## Commit progress

- [x] 1. refactor(cli): unify mapper ownership errors
