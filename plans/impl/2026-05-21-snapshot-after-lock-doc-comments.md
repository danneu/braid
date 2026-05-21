# Plan: pin the snapshot-after-lock invariant via doc comments

## Context

A code-review finding asked for a Rust dispatch unit test that pins the
ordering invariant that `online_state::snapshot()` must be called inside
the pool-lock window. Without that ordering, `Commands::Add` / `Unlock` /
`Recover` could re-introduce the `deactivating` deadlock that ADR 026's
"Snapshot Rule On `systemctl start`" prevents
(`docs/decisions/026-pool-lock-rust-owned.md:73-87`).

The finding's proposed mechanism -- a `RecordingPoolLock` test seam
plus a generic dispatch arm -- directly reverses
`plans/impl/2026-05-20-centralize-locked-command-policy.md`, which
deliberately deleted that exact seam on the grounds that single-impl
traits without test consumers are dead abstraction. The underlying
invariant gap is real, but its right shape is documentation, not a
re-introduced seam, for three reasons:

1. The structural layout (lock at the top of `main()` outside the
   match, snapshot inside each command arm) already makes a hoisting
   refactor visible in a diff.
2. The invariant is already authoritative in ADR 026 (and reinforced
   by ADR 018:173-174) -- it is not documented at the call boundary
   in source.
3. `pub fn snapshot`, `pub struct OnlineSnapshot`, `pub fn mark_online`,
   and `pub fn mark_offline` in `cli/src/online_state.rs` currently
   lack the `///` doc comments that `AGENTS.md` requires for new
   top-level `pub`/`pub(crate)` items. The snapshot-rule cluster
   (producer, carrier, consumer, symmetric finalizer) is the natural
   place to satisfy that convention while pinning the invariant at the
   function and type contracts.

Intended outcome: a future refactor that hoists the snapshot above
the lock has to read past explicit `///` contracts on the producer,
the carrier type that travels into the consumer, the consumer itself,
and the symmetric finalizer -- plus an inline ordering note at the
lock-acquire site. The documentation lives where the refactor edit
lands, not only in ADRs.

## Approach

Add five short comments. No code behavior changes, no test
additions, no test apparatus.

The cluster is producer -> carrier -> consumer -> symmetric finalizer,
plus an inline ordering note at the dispatch lock-acquire site.

### 1. `///` on `cli/src/online_state.rs::snapshot` (line 246)

Producer contract. Capture two facts: (a) it reads
`braid-online.service` ActiveState as the entry-state reading the
post-mutation finalizer (`mark_online`) gates on, and (b) it must be
called at the start of the pool-lock window so the captured reading
defines the entry state of this command's exclusive section. The pool
lock does not make systemd unit state stable in general -- external
`systemctl stop` calls during the window are still possible; the
invariant is "decision uses the entry-state reading", not "state is
frozen". Cite ADR 026's snapshot-rule section by repo-relative path,
matching the project convention (e.g. `cli/src/main.rs:494` references
`docs/decisions/019-inhibit-sleep.md`; `cli/src/main.rs:817` and
`cli/src/remove.rs:454` use the same path style).

One to three lines per `AGENTS.md` doc-comment guidance.

### 2. `///` on `cli/src/online_state.rs::OnlineSnapshot` (line 242)

Carrier contract. Name it as the entry-state `braid-online.service`
ActiveState reading captured by `snapshot()` inside locked dispatch
and consumed later by `mark_online()` to gate its `systemctl start`.
One line is enough -- the producer and consumer doc comments carry
the why; this anchors the type that travels between them and resolves
the AGENTS.md `pub`-item violation on the carrier itself.

### 3. `///` on `cli/src/online_state.rs::mark_online` (line 253)

Consumer contract. Capture that the `start braid-online.service`
decision is gated on the entry-state `OnlineSnapshot` captured at the
start of the same pool-lock window, and that skipping `active` /
`activating` / `deactivating` on the captured reading is what prevents
the start from queuing behind an in-flight stop. Same ADR 026
snapshot-rule path reference.

### 4. `///` on `cli/src/online_state.rs::mark_offline` (line 328)

Symmetric finalizer contract for plain `braid lock`. Capture that the
stop side does NOT use a snapshot gate; it relies on
`/run/braid-stop-coordinator.lock` plus the `done\n` protocol from
ADR 026 § "Stop Coordinator + Done Protocol". This explains why
`mark_offline`'s contract differs from `mark_online`'s -- a reader who
expects a symmetric snapshot rule needs that contrast spelled out.

### 5. Inline ordering note at `cli/src/main.rs:490`

A short `//` line above the `let _pool_guard = acquire_per_policy(...)`
statement noting that the `snapshot(&online_ops)` calls in the Add /
Unlock / Recover arms sit inside this lock window deliberately. Cite
the ADR 026 snapshot rule. This mirrors the existing comment style at
line 492-494 (which references `docs/decisions/019-inhibit-sleep.md`
for the hoisted `sleep_inhibitor`).

The note uses `--`, not the Unicode em-dash, per the project's
ASCII-output rule in `AGENTS.md` § "CLI Output Style". The
pre-existing ADR-019 comment on line 492-494 uses an em-dash; do not
change it as part of this pivot (out of scope).

## Critical files to modify

- `cli/src/online_state.rs` -- four `///` doc-comment blocks at the
  definitions of `pub fn snapshot`, `pub struct OnlineSnapshot`,
  `pub fn mark_online`, and `pub fn mark_offline`. No body changes.
- `cli/src/main.rs` -- one `//` line above
  `let _pool_guard = acquire_per_policy(&pool_lock, lock_policy(&cli.command));`
  at line 490. No control-flow changes.

## Reuse / convention anchors

- ADR-reference comment style: path-relative, e.g. existing
  `cli/src/main.rs:494` -> `docs/decisions/019-inhibit-sleep.md`,
  `cli/src/main.rs:817` -> `docs/decisions/018-systemd-lifecycle.md`,
  `cli/src/remove.rs:454` -> `docs/decisions/014-alerts.md`. Match
  that path style.
- Doc-comment length / intent rules: `AGENTS.md` § "Doc Comments"
  ("Capture intent, invariant, ownership, or call-site coupling --
  not the signature. Prefer one to three lines.").
- Producer/consumer pairing: `OnlineSnapshot` is consumed by
  `mark_online` via its `snap: Option<&OnlineSnapshot>` parameter
  (`cli/src/online_state.rs:254`). The `///` on `snapshot` and the
  `///` on `mark_online` should reference each other implicitly by
  naming the contract (snapshot rule), not by hardcoding line
  numbers.

## Explicitly out of scope

- Resurrecting `RecordingPoolLock` or any dispatch seam (rejected by
  `plans/impl/2026-05-20-centralize-locked-command-policy.md`).
- Extending `tests/module/pool-lock-precedes-state-read.py` with a
  `systemctl`-shim subtest. Considered; rejected as VM-side analogue
  of the deleted seam, adds shadowing/logging plumbing for one
  assertion at one site, contradicts the "no abstractions beyond
  what the task requires" rule for a Low-severity gap.
- Doc-commenting the rest of the `cli/src/online_state.rs` public
  surface (`OnlineStateOps` trait, `UnitActiveState` enum, error
  types, etc.) outside the snapshot-rule cluster. They also violate
  the convention, but they are not part of this producer / carrier /
  consumer / finalizer story -- audit-scope, not pivot-scope.
  (`OnlineSnapshot` itself is in scope -- see Approach section 2.)
- Fixing the pre-existing em-dash in `cli/src/main.rs:494`.

## Verification

The change is documentation-only; behavior is unchanged. A green
build is sufficient evidence.

1. `just test-rust` -- confirms the new doc-comment text doesn't
   accidentally break a doctest or anything else.
2. `cargo clippy --workspace --all-targets -- -D warnings` (from
   `cli/`) -- catches doc-comment lint issues (broken intra-doc
   links, etc.).
3. Manual read: open each of the five edited locations (four in
   `cli/src/online_state.rs`, one in `cli/src/main.rs`) and confirm
   the doc-comment renders as intended via `cargo doc -p braid-cli
   --no-deps --open` (optional; rustdoc render of `online_state`
   pub items).

No VM test run needed; no behavior surface touched.
