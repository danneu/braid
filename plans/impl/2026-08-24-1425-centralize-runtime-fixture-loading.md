# Centralize runtime fixture loading

## Problem

Thirteen parser unit-test modules carry identical readers for the stable
fixture lane. Equivalent readers also exist in the smartctl parser and doctor
test fixtures, while the systemctl parser repeats the same root-resolution and
fail-closed behavior for an unversioned fixture. Commit `9d237f7b` had to update
each stable-path copy during the move to `nixos-26.05`, demonstrating the
maintenance cost.

The integration golden harness already centralizes lane-aware fixture reads,
but its stable-only SMART test bypasses that helper for another direct read.

## Decision

- The existing crate-wide `#[cfg(test)]` fixture boundary will own runtime
  fixture resolution for unit tests. It will support both paths relative to the
  fixture root and names relative to the authoritative stable lane. The
  root-relative path keeps the hand-authored systemctl fixture lane-independent
  because the capture pipeline does not produce it.
- All parser and doctor unit-test runtime readers will use that boundary. This
  includes the 13 identical parser copies, the smartctl and doctor variants,
  and the systemctl root-level variant.
- The stable golden SMART test alone will switch to the golden harness's
  existing lane-aware reader; the shared golden harness remains unchanged.
- Production parser helpers will remain focused on parsing rather than test
  I/O, and no test-only fixture API will enter production builds.

## Invariants

- Runtime fixture contents and parser assertions remain unchanged.
- Missing or unreadable runtime fixtures fail closed with an actionable
  diagnostic that identifies the resolved path and underlying I/O error.
- The stable fixture lane has one runtime source of truth within crate unit
  tests.
- The integration golden harness remains lane-parameterized so stable and
  unstable fixtures continue to exercise the same shared test corpus.
- Existing `include_str!` consumers retain compile-time embedding and
  missing-file detection.

## Proof obligations

- A focused test proves that a missing stable runtime fixture fails closed with
  an actionable path-bearing diagnostic.
- `just test-rust` proves that all migrated unit and stable golden consumers
  continue to load and parse their fixtures successfully.
- A tracked-file search proves that parser-local runtime fixture readers and
  duplicated runtime stable-directory literals are gone, excluding policy
  documentation, the lane-parameterized golden harness, and intentional
  `include_str!` paths.

## Non-goals

- Do not change fixture data, parser behavior, tool pins, or fixture-capture
  workflows.
- Do not merge compile-time fixture embedding into the runtime reader.
- Do not make the crate-internal test fixture boundary available to integration
  tests or production code.
- Do not run the VM parser canaries; this refactor does not change parser logic
  or live tool behavior.

## Accepted risks

- A future parser test could reintroduce a local reader; existing helper reuse,
  review, and the implementation-time inventory check are proportionate for
  test-only duplication, so this plan does not add a dedicated CI checker.

## Implementation discretion

- Helper names, exact signatures, and internal path construction are left to
  implementation so long as the ownership and behavior above hold.
- Exact panic wording is discretionary; fail-closed behavior and actionable
  path plus I/O context are contractual.

## Commit progress

- [x] 1. refactor(test): centralize runtime fixture loading
