# Plan: inject a `Sleeper` into `close_mapper_with_retry`

## Context

`cli/src/lock.rs:25-26` defines:

```rust
const CLOSE_RETRY_ATTEMPTS: u32 = 3;
const CLOSE_RETRY_DELAY: Duration = Duration::from_millis(500);
```

`close_mapper_with_retry` calls `thread::sleep(CLOSE_RETRY_DELAY)` between
attempts. Several unit tests pump busy `CryptsetupClose` responses through
`MockRunner`, driving the full retry loop and paying real wall-clock sleep:

| Test                                            | Busy mappers | Real sleeps | Wall cost |
| ----------------------------------------------- | ------------ | ----------- | --------- |
| `lock_umount_busy_fails`                        | 2            | 2 x 2       | ~2.0s     |
| `lock_umount_busy_includes_hint`                | 2            | 2 x 2       | ~2.0s     |
| `lock_umount_fails_busy_mapper_is_warning`      | 2            | 2 x 2       | ~2.0s     |
| `lock_umount_fails_orphan_busy_is_warning`      | 1            | 2           | ~1.0s     |
| `lock_retries_busy_close_then_succeeds`         | 1 (1 retry)  | 1           | ~0.5s     |

Total: roughly **7.5s** of real sleep in `cargo test -p braid-cli`. Under
`MockRunner` there is no kernel race for the delay to mitigate -- the sleep
is pure waste.

Why not `#[cfg(test)] const CLOSE_RETRY_DELAY = 0`? That creates test-vs-prod
build divergence: no single test binary ever exercises the production timing
value, so no deterministic test can verify "prod sleeps 500ms between busy
attempts." The only candidate backstops both fail that bar:

- `tests/repro/cryptsetup-close-btrfs-held.py` -- **not** a braid test. Has
  no braid dependency (see `cryptsetup-close-btrfs-held.nix:8`), runs raw
  `cryptsetup`/`btrfs` commands only, and its pre-forget close is explicitly
  race-dependent (see `cryptsetup-close-btrfs-held.py:50-61`: "may succeed
  or fail depending on timing"). It documents kernel behavior, not
  `braid lock`'s retry.
- `tests/cli/braid-lock-btrfs-held.py` -- does run `braid lock` end-to-end
  (3 lock/unlock cycles on a 3-disk pool), but depends on the same timing
  race to trigger the retry path. A regression that removed or zeroed the
  sleep would only surface probabilistically.

Dependency injection fixes both at once: zero wall-cost for unit tests AND a
single deterministic unit test that pins the prod delay.

## Approach

Introduce a `Sleeper` trait (private to the crate). `close_mapper_with_retry`
takes an impl. A `RealSleeper` calls `thread::sleep` in production. Tests
drive through `cmd_lock_impl` (a private variant of `cmd_lock`) with a
`NoopSleeper`, except the new timing-lock test, which uses a
`RecordingSleeper` to capture the exact `Duration` arguments.

Public `cmd_lock` stays a thin wrapper that passes `&RealSleeper`. The single
prod callsite at `cli/src/main.rs:520` is unchanged.

`CLOSE_RETRY_ATTEMPTS` remains a plain const. Its behavioral contract is
already pinned by `lock_retries_busy_close_then_succeeds` (asserts
`aaa_calls == 2`) and called out in that test's comment at
`cli/src/lock.rs:1329`.

## Changes

### 1. `cli/src/lock.rs` -- trait and production sleeper

Add after the existing constants (all private to `lock.rs` -- the nested
`tests` module accesses parent-private items without any visibility
annotation):

```rust
trait Sleeper {
    fn sleep(&self, duration: Duration);
}

struct RealSleeper;

impl Sleeper for RealSleeper {
    fn sleep(&self, duration: Duration) {
        thread::sleep(duration);
    }
}
```

Change `close_mapper_with_retry`:

```rust
fn close_mapper_with_retry<R: CommandRunner, S: Sleeper>(
    runner: &R,
    sleeper: &S,
    mapper: &str,
) -> Result<(), LockError> {
    // ... existing body ...
    sleeper.sleep(CLOSE_RETRY_DELAY);
    // ... unchanged rest ...
}
```

Split `cmd_lock` into public wrapper + private impl:

```rust
pub fn cmd_lock<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    config: &Config,
    membership: &PoolMembership,
    dry_run: bool,
) -> Result<(), LockError> {
    cmd_lock_impl(runner, fs, &RealSleeper, config, membership, dry_run)
}

fn cmd_lock_impl<R, F, S>(
    runner: &R,
    fs: &F,
    sleeper: &S,
    config: &Config,
    membership: &PoolMembership,
    dry_run: bool,
) -> Result<(), LockError>
where
    R: CommandRunner,
    F: Filesystem + ?Sized,
    S: Sleeper,
{
    // existing cmd_lock body; pass `sleeper` to each
    // close_mapper_with_retry(runner, sleeper, ...) call
}
```

`main.rs:520` is unchanged (still calls `cmd_lock`). No other callers. The
private `Sleeper`/`RealSleeper`/`cmd_lock_impl` items are not exported from
`lib.rs` and do not widen the crate's public API.

### 2. `cli/src/lock.rs` tests -- `NoopSleeper` + call-site update

Add to the `tests` module:

```rust
struct NoopSleeper;
impl Sleeper for NoopSleeper {
    fn sleep(&self, _duration: Duration) {}
}
```

Every existing `cmd_lock(&runner, &fs, &config, &membership, <dry>)` call in
the `tests` module becomes:

```rust
cmd_lock_impl(&runner, &fs, &NoopSleeper, &config, &membership, <dry>)
```

This is a mechanical rename/arg-add across ~22 call sites in this module
(`lock.rs:636`, `651`, `675`, `723`, `771`, `800`, `838`, `879`, `929`, `978`,
`1021`, `1058`, `1091`, `1141`, `1187`, `1231`, `1266`, `1305`, `1361`,
`1394`, `1419`, plus any missed by the grep). Use `grep -n 'cmd_lock(' cli/src/lock.rs`
to enumerate before editing.

### 3. `cli/src/lock.rs` tests -- new deterministic prod-delay lock

Add to the `tests` module:

```rust
/*
 * Intent: close_mapper_with_retry sleeps exactly CLOSE_RETRY_DELAY between
 *   busy attempts, and the prod value of CLOSE_RETRY_DELAY remains 500ms.
 *
 * Why it exists: the retry delay papers over a kernel-level race between
 *   umount and cryptsetup close on multi-device btrfs (see commit 1484ff1
 *   and tests/repro/cryptsetup-close-btrfs-held.py). The repro test is
 *   race-dependent and the CLI-level VM test braid-lock-btrfs-held.py
 *   relies on the same race to trigger the retry path -- neither
 *   deterministically catches a regression that removes, zeroes, or
 *   bypasses the sleep. This test locks the contract at the helper.
 *
 * Scenario: a busy close error repeats for all CLOSE_RETRY_ATTEMPTS tries;
 *   the RecordingSleeper captures (CLOSE_RETRY_ATTEMPTS - 1) sleep calls,
 *   each exactly CLOSE_RETRY_DELAY, and the returned error is DeviceBusy.
 */
#[test]
fn close_mapper_with_retry_sleeps_prod_delay_between_busy_attempts() {
    struct RecordingSleeper(Mutex<Vec<Duration>>);
    impl Sleeper for RecordingSleeper {
        fn sleep(&self, d: Duration) {
            self.0.lock().unwrap().push(d);
        }
    }

    let sleeper = RecordingSleeper(Mutex::new(Vec::new()));
    let runner = MockRunner::default().with_output(
        CmdRequest::CryptsetupClose { mapper: "braid-aaa".into() },
        err_raw(
            "cryptsetup close braid-aaa",
            5,
            "Device braid-aaa is still in use.",
        ),
    );

    let err = close_mapper_with_retry(&runner, &sleeper, "braid-aaa")
        .expect_err("should exhaust retries and return DeviceBusy");
    assert!(
        matches!(err, LockError::DeviceBusy(_)),
        "expected DeviceBusy after retry exhaustion, got: {err:?}"
    );

    let recorded = sleeper.0.lock().unwrap().clone();
    assert_eq!(
        recorded.len(),
        (CLOSE_RETRY_ATTEMPTS - 1) as usize,
        "expected one sleep between each pair of attempts, got: {recorded:?}"
    );
    for d in &recorded {
        assert_eq!(
            *d, CLOSE_RETRY_DELAY,
            "each retry must sleep CLOSE_RETRY_DELAY, got: {recorded:?}"
        );
    }
    assert_eq!(
        CLOSE_RETRY_DELAY,
        Duration::from_millis(500),
        "prod CLOSE_RETRY_DELAY must stay 500ms; if you intend to change \
         this, update the kernel-race justification in the commit message"
    );
}
```

This test runs in microseconds and fails under every helper-level regression
the `#[cfg(test)] const` approach left uncovered:

- Sleep silently removed from `close_mapper_with_retry` -- `recorded.len()`
  is 0.
- Sleep passed the wrong `Duration` (e.g. a hardcoded `Duration::ZERO`) --
  equality check fails.
- Prod const flipped to a shorter value -- literal 500ms assertion fails.
- Retry count changed -- `recorded.len()` check fails, AND
  `lock_retries_busy_close_then_succeeds` fails independently.

However, the helper-level test does not prove the **wrapper wiring**: that
public `cmd_lock` passes `&RealSleeper` rather than a noop. A regression
that swapped `&RealSleeper` for `&NoopSleeper` (or dropped the sleep
altogether) in the production path would pass both the helper test and the
race-dependent VM test. That gap is closed below.

### 4. `cli/src/lock.rs` tests -- wrapper wiring lock

Add to the `tests` module:

```rust
/*
 * Intent: public cmd_lock wires a real sleeper. An always-busy mapper
 *   makes the wrapper pay measurable wall-clock sleep time before
 *   returning DeviceBusy, proving &RealSleeper (not &NoopSleeper) is on
 *   the hot path.
 *
 * Why it exists: the helper-level RecordingSleeper test proves
 *   close_mapper_with_retry uses CLOSE_RETRY_DELAY, but does not prove the
 *   public wrapper hands in &RealSleeper. A regression that ships
 *   &NoopSleeper (or drops the sleeper entirely) in production would
 *   leave lock reliability race-dependent and pass every helper-level
 *   unit test -- including braid-lock-btrfs-held.py, which only asserts
 *   success and does not deterministically force the retry path.
 *
 * Scenario: umount succeeds, then every mapper close returns "is still
 *   in use" so the retry loop runs to exhaustion. Because umount did not
 *   set umount_error, DeviceBusy is NOT suppressed: it becomes
 *   first_mapper_error and is the returned value. Wall time is bounded
 *   below by (CLOSE_RETRY_ATTEMPTS - 1) * CLOSE_RETRY_DELAY for a single
 *   mapper; we assert a tolerant lower bound of that amount to stay
 *   robust on slow CI while still failing loudly if no real sleep
 *   happened.
 */
#[test]
fn cmd_lock_wrapper_uses_real_sleeper() {
    use std::time::Instant;

    let runner = mounted_runner()
        .with_output(
            CmdRequest::CryptsetupClose { mapper: "braid-aaa".into() },
            err_raw(
                "cryptsetup close braid-aaa",
                5,
                "Device braid-aaa is still in use.",
            ),
        )
        .with_output(
            CmdRequest::CryptsetupClose { mapper: "braid-bbb".into() },
            err_raw(
                "cryptsetup close braid-bbb",
                5,
                "Device braid-bbb is still in use.",
            ),
        );
    let fs = MockFs::new(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
    let config = test_config();
    let membership = test_membership();

    let start = Instant::now();
    let err = cmd_lock(&runner, &fs, &config, &membership, false)
        .expect_err("should fail with DeviceBusy after retry exhaustion");
    let elapsed = start.elapsed();

    assert!(
        matches!(err, LockError::DeviceBusy(_)),
        "expected DeviceBusy from public wrapper, got: {err:?}"
    );

    // Both mappers hit the full retry loop: expected total real sleep is
    // 2 * (CLOSE_RETRY_ATTEMPTS - 1) * CLOSE_RETRY_DELAY = 2s. We assert a
    // tolerant lower bound of one mapper's worth (~900ms) so scheduler
    // jitter on slow CI does not cause flake, while still catching a
    // NoopSleeper regression (which would complete in microseconds).
    let min_expected =
        CLOSE_RETRY_DELAY * (CLOSE_RETRY_ATTEMPTS - 1) - Duration::from_millis(100);
    assert!(
        elapsed >= min_expected,
        "wrapper must use RealSleeper -- elapsed {:?} < min {:?}",
        elapsed,
        min_expected,
    );
}
```

This test costs real wall time on purpose (~2s). It is the only test in the
module that pays the full retry delay, and it buys deterministic coverage
for the wrapper-wiring regression that is otherwise race-dependent.

## Files modified

- `cli/src/lock.rs` -- add private `Sleeper` trait + `RealSleeper`; thread
  sleeper through `close_mapper_with_retry`; split `cmd_lock` into public
  wrapper and private `cmd_lock_impl`; update all in-module test call sites
  to use `cmd_lock_impl` + `NoopSleeper`; add the helper-level prod-delay
  test and the wrapper-level real-sleeper test.
- No other source file changes. `cli/src/main.rs:520` unchanged.

## Verification

1. `just test-rust` -- all tests pass, including the new
   `close_mapper_with_retry_sleeps_prod_delay_between_busy_attempts` and
   `cmd_lock_wrapper_uses_real_sleeper`.
2. Wall-clock comparison before/after on the lock module:
   `cargo test -p braid-cli lock:: -- --test-threads=1`
   - Before: ~7.5s across the 5 busy-path tests (each paying 1-2s of
     real sleep).
   - After: ~2s, dominated by the single new
     `cmd_lock_wrapper_uses_real_sleeper` test. The 22 existing tests drop
     to sub-second combined.
3. `just test-vm braid-lock-btrfs-held` -- end-to-end `braid lock` path
   still closes all mappers across 3 lock/unlock cycles on a 3-disk pool.
   Not relied on as wiring proof (race-dependent); it is a generalized
   regression backstop.
4. `cargo clippy -p braid-cli --tests` -- no new warnings.
