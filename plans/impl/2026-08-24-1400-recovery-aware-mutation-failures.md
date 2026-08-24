# Recovery-aware mutation failures

## Problem

`remove-missing` reports failures after its btrfs membership commit through a
generic validation error even though `pending-op.json` has placed the system in
recovery mode. The same gap recurs in `add` and `replace`, while `remove` carries
its own partial error taxonomy. Operators can therefore receive a low-level
failure without being told whether to retry the original command, run `braid
recover`, or repair state storage first.

The relevant lifecycle is already fixed by
[Principle 3](../../docs/design/principles.md#3-safe-by-construction-operations),
[ADR 017](../../docs/design/decisions/017-runtime-disk-membership.md#mutation-ordering),
and the
[mutation safety heuristics](../../docs/dev/safety-heuristics.md): journal the
intent, perform the membership mutation, persist committed membership, advance
the journal when the operation has post-commit maintenance, finish that
maintenance, then clear the journal.

The pending journal is the authoritative observable for recovery mode. The
command boundary can derive remediation from that state directly instead of
threading lifecycle tags through every fallible operation.

## Decision

Classify mutation failures once at the command boundary for `add`, `remove`,
`remove-missing`, and `replace`. Planning and other failures before the initial
journal-write attempt render unchanged. Once an execution attempts that write,
it records whether the write completed successfully; on error, the boundary
preserves the original subsystem-specific content, observes `pending-op.json`
through `journal::load_journal`, and appends one shared remediation:

- A valid journal is visible: the operation may be partial; run `braid recover`
  before retrying the original mutation.
- No journal is visible after the initial journal-write attempt failed: no
  mutation started; repair the reported write fault and retry the original
  command.
- No journal is visible after the command completed its initial journal write:
  the final clear was reached but deletion durability was not confirmed; repair
  state-directory I/O, and run recovery only if the journal reappears after a
  restart.
- The journal is unreadable or unparseable: report the observed journal error
  and the existing manual/storage remediation. Do not direct the operator into
  `braid recover`, which cannot consume that journal state.

Existing error variants that embed lifecycle remediation must surrender that
part of their wording to the boundary classifier. They retain distinct
subsystem facts and remediation, such as `pool.json` uncertainty or required
acked-stats cleanup, but must not carry their own `braid recover` instruction.
The classifier is the single source of retry-versus-recover guidance.

This classifier also wraps recovery execution. Every failure from `braid
recover`, including all journal-clear paths, is classified from the journal
state observed at its command boundary: a valid visible journal says to rerun
recovery, an absent journal after recovery work reports unconfirmed deletion
durability, and an unreadable journal uses manual/storage remediation.

The final journal clear remains the last fallible state transition in each
successful mutator and recovery branch. Cleanup after clear may remain
best-effort and non-fatal; no new fallible work may make an absent journal
ambiguous with an unrelated later failure.

## Invariants

- Every error after the initial journal-write attempt is classified at its
  command boundary, so a post-journal subsystem error cannot escape without
  recovery context; earlier planning and validation errors remain unchanged.
- Remediation comes from `journal::load_journal`, never a path-existence probe
  or a caller-maintained lifecycle phase.
- A valid visible journal always directs the operator to `braid recover` before
  retrying a normal mutation.
- `braid recover` is not suggested when no journal was installed or when the
  journal cannot be read and parsed by recovery.
- The original subsystem facts and specific remediation, including ENOSPC,
  scrub, mapper-identity, LUKS, `pool.json`, and acked-stats guidance, remain
  intact before the shared lifecycle advice, but preexisting lifecycle advice
  is removed so the result cannot tell the operator both to recover and not to
  recover.
- Error wrappers remain role-based and transparent at the command boundary so
  `print_cli_error` continues to add the only leading `error: ` marker.
- User-facing output is ASCII-only.
- Journal and membership schemas, mutation ordering, recovery replay decisions,
  command exit codes, confirmation behavior, and dry-run output do not change.

## Proof obligations

- Prove the shared classifier covers valid-visible, absent-after-failed-install,
  absent-after-successful-install, unreadable, and unparseable journal
  observations without contradictory advice.
- For each of the four mutators, prove a pre-attempt failure renders unchanged
  and does not mention `braid recover`, while a post-install failure preserves
  the journal and directs the operator to recovery.
- Prove the command-boundary composition for an absent journal after successful
  install renders deletion-durability guidance and does not retain an embedded
  `braid recover` instruction from the terminal-clear error.
- Prove representative persistence and maintenance failures retain their
  original subsystem detail while gaining the same journal-derived advice.
- Prove initial journal-write failures are classified from the journal actually
  observable after the failed write, including the case where the file became
  visible before durability confirmation failed.
- Prove every recovery failure, including every branch that attempts journal
  clear, passes through the shared classifier and renders advice from the
  observed final journal state.
- Prove existing journal phases, journal-survival behavior, and `pool.json`
  ordering remain unchanged for add, remove, remove-missing, replace, and
  recover.
- Verify the Rust suite, the CLI ASCII guard, and the documentation build.

## Documentation

Update the mutation safety heuristics to require authoritative, command-boundary
classification for journal-bearing failures and to keep final journal clear as
the terminal fallible state transition. Document the visible, absent, and
unreadable journal outcomes in the recovery scenarios guide and keep the
README's recovery summary synchronized.

## Non-goals

- Do not change state-I/O error types or add rename/unlink durability state.
- Do not change on-disk membership or journal formats.
- Do not redesign `braid recover` or add automatic recovery to normal
  mutations.
- Do not add new retry behavior inside failed mutations; the journal remains
  the handoff boundary.

## Accepted risks

- A future fallible operation added after final journal clear could be
  misclassified as uncertain deletion; this remains review-enforced through the
  documented terminal-clear contract because adding coordination or test-only
  state-I/O seams is disproportionate to a non-destructive guidance regression.

## Implementation discretion

- The shared classifier's internal type, module, and wrapper arrangement are
  discretionary as long as classification occurs once at every command
  boundary and preserves subsystem-specific error content.

## Commit progress

- [x] 1. fix(cli): classify journal-bearing mutation failures
- [x] 2. fix(recover): classify recovery failures from journal state
