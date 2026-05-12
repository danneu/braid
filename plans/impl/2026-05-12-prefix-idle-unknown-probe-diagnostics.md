# Prefix `braid idle` Unknown Probe Diagnostics

## Summary

Add probe-source labels to `BusyReason::Unknown` messages so `braid idle`
reports which fail-closed probe failed, e.g. `busy: unknown (sysfs: cannot
read exclusive operation status: ...)`. Build on the current staged
sysfs-before-scrub order and do not revert it.

## Interface Changes

- No Rust public type changes: keep `IdleResult` and `BusyReason` as-is.
- User-facing CLI output changes only for unknown busy results:
  - `mountinfo: ...` for mount presence failures.
  - `sysfs: ...` for `/sys/fs/btrfs/*/exclusive_operation` scan/read/parse failures.
  - `scrub: ...` for `btrfs scrub status` invocation or parse failures.

## Implementation Changes

- Change private helper in `cli/src/idle.rs` to
  `busy_unknown(layer: &str, e: impl Display)` and format `"{layer}: {e}"`.
- Update the four call sites:
  - `is_btrfs_mounted` error -> `busy_unknown("mountinfo", e)`.
  - `check_any_btrfs_exclusive_op` error -> `busy_unknown("sysfs", e)`.
  - `runner.run(BtrfsScrubStatus)` error -> `busy_unknown("scrub", e)`.
  - `parse_btrfs_scrub_status` error -> `busy_unknown("scrub", e)`.
- Add an idle fixture assertion helper that checks the `BusyReason::Unknown`
  message prefix. Remove the old classification-only helper if no
  exact-text-irrelevant call sites remain after the prefix assertions are added.
- Update `manual/commands/idle.md` so `busy: unknown` documents the new
  `<probe>: <error>` shape and the three probe labels, and rewrite the
  "What happens under the hood" numbered flow to match current behavior:
  mountinfo -> sysfs exclusive-op scan -> scrub, with sysfs busy/unknown
  short-circuiting before the scrub probe.

## Test Plan

- Update existing idle unit tests so each fail-closed branch asserts the
  expected prefix:
  - mountinfo read/malformed failures -> `mountinfo:`.
  - sysfs unrecognized/read/list/empty/notfound failures -> `sysfs:`.
  - scrub missing mock/invocation failure -> `scrub:`.
- Add a unit test for scrub parse failure after a clean sysfs scan, asserting
  `scrub:`.
- Update `tests/cli/braid-idle.py` to expect `busy: unknown (scrub:` while
  still asserting the underlying simulated scrub failure is preserved.
- Run `just test-rust`.
- Run `just test-vm braid-idle`.

## Assumptions

- The best fix is the labeled helper, not inlining every call site and not a
  blanket `From<E> for IdleResult`, because the source label is call-site
  context.
- `braid idle` prints busy status to stdout today; this plan preserves that
  output stream and only improves the message body.
